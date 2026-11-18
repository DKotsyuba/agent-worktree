#!/usr/bin/env bash
# Authenticated/public bootstrap for the single-binary-v1 profile. No tar extraction.
# One raw binary per supported target; the host's payload is selected, never guessed.
set +x
set -euo pipefail
umask 077
PRODUCT='agent-worktree'
REPO='DKotsyuba/agent-worktree'
version=''
home=${AGENT_WORKTREE_HOME:-"$HOME/.$PRODUCT"}
bin_dir="$HOME/.local/bin"
while (($#)); do
  case "$1" in
    --help) echo 'install.sh --version X.Y.Z [--repo OWNER/REPO] [--home ABS_PATH] [--bin-dir ABS_PATH]'; exit 0;;
    --version|--repo|--home|--bin-dir)
      (($# >= 2)) || { echo 'Missing option value' >&2; exit 2; }
      case "$1" in --version) version=$2;; --repo) REPO=$2;; --home) home=$2;; --bin-dir) bin_dir=$2;; esac
      shift 2;;
    *) echo 'Unknown installation option' >&2; exit 2;;
  esac
done
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.-]+)?$ ]] || { echo 'Use an explicit version' >&2; exit 2; }
[[ "$REPO" =~ ^[A-Za-z0-9][A-Za-z0-9-]*/[A-Za-z0-9][A-Za-z0-9_.-]*$ ]] || { echo 'Repository is missing or invalid' >&2; exit 2; }
[[ "$home" == /* && "$bin_dir" == /* ]] || { echo 'Installation paths must be absolute' >&2; exit 2; }
case "$(uname -s):$(uname -m)" in
  Darwin:arm64) target=aarch64-apple-darwin;;
  Linux:x86_64)
    # The Linux payload links glibc; refuse hosts (musl, Alpine) where it cannot run.
    getconf GNU_LIBC_VERSION >/dev/null 2>&1 || { echo 'The Linux payload needs glibc' >&2; exit 2; }
    target=x86_64-unknown-linux-gnu;;
  *) echo 'Unsupported host: payloads exist for macOS arm64 and Linux x86_64 (glibc) only' >&2; exit 2;;
esac
asset="$PRODUCT-$target"
temp=$(mktemp -d "${TMPDIR:-/tmp}/agent-mcp-install.XXXXXXXX")
trap 'rm -rf -- "$temp"' EXIT
mkdir "$temp/bundle"
authenticated=0
if command -v gh >/dev/null && gh auth status --hostname github.com >/dev/null 2>&1; then authenticated=1; fi
download() {
  local name=$1 output=$2 max=$3
  if ((authenticated)); then
    local size
    size=$(GH_HOST=github.com gh release view "v$version" --repo "$REPO" --json assets --jq ".assets[] | select(.name == \"$name\") | .size")
    [[ "$size" =~ ^[0-9]+$ ]] && ((size <= max)) || { echo 'Invalid or excessive release asset size' >&2; return 1; }
    GH_HOST=github.com gh release download "v$version" --repo "$REPO" --pattern "$name" --output "$output"
  else
    curl --disable --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
      --connect-timeout 15 --max-time 300 --max-filesize "$max" \
      "https://github.com/$REPO/releases/download/v$version/$name" --output "$output"
  fi
  (($(wc -c < "$output") <= max)) || return 1
}
sha256_of() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi
}
download SHA256SUMS "$temp/SHA256SUMS" 8192
download release-manifest.json "$temp/bundle/release-manifest.json" 16384
download "$asset" "$temp/bundle/$asset" 536870912
for name in "$asset" release-manifest.json; do
  expected=$(awk -v n="$name" '$2 == n {count++; hash=$1} END {if(count != 1) exit 1; print hash}' "$temp/SHA256SUMS")
  [[ "$expected" =~ ^[0-9a-f]{64}$ ]] || { echo 'Invalid checksum entry' >&2; exit 1; }
  actual=$(sha256_of "$temp/bundle/$name")
  [[ "$actual" == "$expected" ]] || { echo 'Checksum mismatch; no candidate executed' >&2; exit 1; }
done
# The manifest must list this host's payload with the same size and hash (old single-payload and
# new multi-payload manifests share the per-payload layout). The binary re-validates all of it.
actual=$(sha256_of "$temp/bundle/$asset")
size=$(wc -c < "$temp/bundle/$asset" | tr -d ' ')
awk -v asset="$asset" -v sha="$actual" -v size="$size" '
  /"binary":/ {listed = index($0, "\"" asset "\"") > 0; hit_sha = hit_size = 0; next}
  listed && /"size":/ {hit_size = index($0, "\"size\": " size ",") > 0}
  listed && /"sha256":/ {hit_sha = index($0, "\"" sha "\"") > 0}
  listed && hit_size && hit_sha {ok = 1}
  END {exit ok ? 0 : 1}' "$temp/bundle/release-manifest.json" || { echo 'Manifest does not list this host payload as downloaded; no candidate executed' >&2; exit 1; }
# Checksums are not signatures. Trust is the authenticated GitHub origin / explicitly selected public origin.
unset GH_TOKEN GITHUB_TOKEN GH_ENTERPRISE_TOKEN GITHUB_ENTERPRISE_TOKEN
mkdir -p "$bin_dir"
chmod 700 "$temp/bundle/$asset"
"$temp/bundle/$asset" self-install --bundle "$temp/bundle" --home "$home" --bin-dir "$bin_dir"
