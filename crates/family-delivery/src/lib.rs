//! Immutable local delivery for the single-binary profile, with or without
//! local state. A release declares the state schema it expects; activation
//! refuses to run an older schema against newer on-disk state, and never
//! touches anything outside `<home>/standalone/`.
//! Hash verification proves integrity, not provenance. Callers authenticate downloads.
#![forbid(unsafe_code)]
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Delivery errors never contain credentials or raw provider bodies.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
/// Only one bounded executable and a small manifest are accepted.
pub const MAX_BINARY_BYTES: u64 = 512 * 1024 * 1024;
/// Targets this consumer knows how to install; anything else is refused.
pub const SUPPORTED_TARGETS: [&str; 2] = ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"];
/// Largest accepted `release-manifest.json`.
pub const MAX_MANIFEST_BYTES: u64 = 16384;
/// Most payloads one release manifest may list.
pub const MAX_PAYLOADS: usize = 8;
/// The product's current on-disk state schema (`state/v1`). Packaging stamps
/// this into new manifests; keep it in lockstep with the store layout.
pub const CURRENT_STATE_SCHEMA: u32 = 1;

/// One payload together with the release identity it belongs to: the flat
/// single-binary view. Version-1 manifests are exactly this shape on disk;
/// version-2 manifests list several payloads (`ReleaseManifest`), and
/// `ReleaseManifest::select` projects one of them back into this view.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Version of the file contract this view was read from (1 or 2).
    pub schema_version: u32,
    /// Must equal single-binary-v1.
    pub profile: String,
    /// Product/binary identity.
    pub product: String,
    /// Immutable product release version.
    pub version: String,
    /// Exact source commit, not a branch.
    pub source_commit: String,
    /// Rust target triple.
    pub target: String,
    /// Bare release asset name.
    pub binary: String,
    /// Exact byte length.
    pub size: u64,
    /// SHA-256 of the executable.
    pub sha256: String,
    /// State schema the release expects: 0 = stateless, 1 = `CURRENT_STATE_SCHEMA`.
    pub state_schema: u32,
    /// GitHub run identity, when built there.
    pub run_id: Option<u64>,
    /// GitHub run attempt, when built there.
    pub run_attempt: Option<u64>,
}
/// Restrict path components; callers must never sanitize a component into a different identity.
pub fn component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn regular(path: &Path) -> Result<()> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err("expected a regular file, not a link".into());
    }
    Ok(())
}
/// Compute a bounded streaming digest without reading the entire binary into memory.
pub fn digest(path: &Path) -> Result<(u64, String)> {
    regular(path)?;
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let (mut total, mut buffer) = (0u64, [0u8; 65536]);
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total = total.checked_add(n as u64).ok_or("size overflow")?;
        if total > MAX_BINARY_BYTES {
            return Err("binary exceeds size limit".into());
        }
        hasher.update(&buffer[..n]);
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}
impl Manifest {
    /// Check owned fields before using any path or identity from the manifest.
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.schema_version, 1 | 2)
            || self.profile != "single-binary-v1"
            || self.state_schema > CURRENT_STATE_SCHEMA
        {
            return Err("unsupported delivery or state profile".into());
        }
        if !self.product.starts_with("agent-")
            || !component(&self.product)
            || !component(&self.version)
            || !component(&self.target)
            || !component(&self.binary)
            || self.binary != format!("{}-{}", self.product, self.target)
            || self.source_commit.len() != 40
            || !self.source_commit.bytes().all(|b| b.is_ascii_hexdigit())
            || self.sha256.len() != 64
            || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || self.size == 0
            || self.size > MAX_BINARY_BYTES
            || self.run_id.is_some() != self.run_attempt.is_some()
            || self.run_id == Some(0)
            || self.run_attempt == Some(0)
        {
            return Err("invalid delivery manifest".into());
        }
        Ok(())
    }
}
/// One payload of a multi-target release.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Payload {
    /// Rust target triple.
    pub target: String,
    /// Bare release asset name: `<product>-<target>`.
    pub binary: String,
    /// Exact byte length.
    pub size: u64,
    /// SHA-256 of the executable.
    pub sha256: String,
}
/// The release manifest: release identity plus every payload. Version 2 is the
/// native format; version-1 single-payload files are still read and appear as
/// a release with one payload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    /// 2 for files written today; 1 only for a legacy single-payload file.
    pub schema_version: u32,
    /// Must equal single-binary-v1.
    pub profile: String,
    /// Product/binary identity.
    pub product: String,
    /// Immutable product release version.
    pub version: String,
    /// Exact source commit, not a branch.
    pub source_commit: String,
    /// State schema the release expects: 0 = stateless, 1 = `CURRENT_STATE_SCHEMA`.
    pub state_schema: u32,
    /// GitHub run identity, when built there.
    pub run_id: Option<u64>,
    /// GitHub run attempt, when built there.
    pub run_attempt: Option<u64>,
    /// Every payload, at most one per target.
    pub payloads: Vec<Payload>,
}
/// The target triple of the running executable, or an error for hosts that
/// have no supported payload. Never derived from untrusted input.
pub fn host_target() -> Result<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok("aarch64-apple-darwin")
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "gnu"
    )) {
        Ok("x86_64-unknown-linux-gnu")
    } else {
        Err("unsupported host target".into())
    }
}
impl ReleaseManifest {
    /// Parse and validate a manifest of schema version 1 (single payload) or 2.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        #[derive(Deserialize)]
        struct Probe {
            schema_version: u32,
        }
        let release = match serde_json::from_slice::<Probe>(bytes)?.schema_version {
            1 => {
                let m: Manifest = serde_json::from_slice(bytes)?;
                Self {
                    schema_version: 1,
                    profile: m.profile,
                    product: m.product,
                    version: m.version,
                    source_commit: m.source_commit,
                    state_schema: m.state_schema,
                    run_id: m.run_id,
                    run_attempt: m.run_attempt,
                    payloads: vec![Payload {
                        target: m.target,
                        binary: m.binary,
                        size: m.size,
                        sha256: m.sha256,
                    }],
                }
            }
            2 => serde_json::from_slice(bytes)?,
            _ => return Err("unsupported delivery or state profile".into()),
        };
        release.validate()?;
        Ok(release)
    }
    fn view(&self, payload: &Payload) -> Manifest {
        Manifest {
            schema_version: self.schema_version,
            profile: self.profile.clone(),
            product: self.product.clone(),
            version: self.version.clone(),
            source_commit: self.source_commit.clone(),
            target: payload.target.clone(),
            binary: payload.binary.clone(),
            size: payload.size,
            sha256: payload.sha256.clone(),
            state_schema: self.state_schema,
            run_id: self.run_id,
            run_attempt: self.run_attempt,
        }
    }
    /// Every payload as a flat, validated single-binary view.
    pub fn views(&self) -> Vec<Manifest> {
        self.payloads.iter().map(|p| self.view(p)).collect()
    }
    /// Check identity and every payload; one payload per target, none duplicated.
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.schema_version, 1 | 2)
            || (self.schema_version == 1 && self.payloads.len() != 1)
            || self.payloads.is_empty()
            || self.payloads.len() > MAX_PAYLOADS
        {
            return Err("invalid delivery manifest".into());
        }
        for (index, payload) in self.payloads.iter().enumerate() {
            self.view(payload).validate()?;
            if self.payloads[..index]
                .iter()
                .any(|p| p.target == payload.target || p.binary == payload.binary)
            {
                return Err("duplicate payload target".into());
            }
        }
        Ok(())
    }
    /// The payload for `target`. Targets this consumer does not support, and
    /// targets the release does not list, are refused rather than guessed.
    pub fn select(&self, target: &str) -> Result<Manifest> {
        if !SUPPORTED_TARGETS.contains(&target) {
            return Err("unsupported target".into());
        }
        self.payloads
            .iter()
            .find(|p| p.target == target)
            .map(|p| self.view(p))
            .ok_or_else(|| "release has no payload for this target".into())
    }
}
fn read_manifest(bundle: &Path) -> Result<ReleaseManifest> {
    if !fs::symlink_metadata(bundle)?.is_dir() {
        return Err("bundle must be a real directory".into());
    }
    let manifest_path = bundle.join("release-manifest.json");
    regular(&manifest_path)?;
    if fs::metadata(&manifest_path)?.len() > MAX_MANIFEST_BYTES {
        return Err("manifest too large".into());
    }
    ReleaseManifest::parse(&fs::read(&manifest_path)?)
}
/// The directory holds exactly the manifest and `binaries`, no links or extras.
fn inventory(bundle: &Path, binaries: &[&str]) -> Result<()> {
    let entries: Vec<_> = fs::read_dir(bundle)?.collect::<std::io::Result<_>>()?;
    if entries.len() != binaries.len() + 1
        || entries.iter().any(|e| {
            e.file_name() != "release-manifest.json"
                && !binaries.iter().any(|b| e.file_name() == *b)
        })
    {
        return Err("unexpected bundle inventory".into());
    }
    Ok(())
}
fn check_payload(bundle: &Path, payload: &Payload) -> Result<()> {
    let (size, sha) = digest(&bundle.join(&payload.binary))?;
    if size != payload.size || sha != payload.sha256 {
        return Err("binary integrity mismatch".into());
    }
    Ok(())
}
/// Validate a directory holding the manifest and every payload it lists.
/// Extra files, links and mismatching bytes are refused.
pub fn verify_release(bundle: &Path) -> Result<ReleaseManifest> {
    let release = read_manifest(bundle)?;
    let names: Vec<&str> = release.payloads.iter().map(|p| p.binary.as_str()).collect();
    inventory(bundle, &names)?;
    for payload in &release.payloads {
        check_payload(bundle, payload)?;
    }
    Ok(release)
}
/// Validate a directory holding the manifest and only the payload for
/// `target` (an installed release or a single downloaded payload). Unknown
/// targets and targets the manifest does not list are refused.
pub fn verify_target(bundle: &Path, target: &str) -> Result<Manifest> {
    let release = read_manifest(bundle)?;
    let selected = release.select(target)?;
    inventory(bundle, &[selected.binary.as_str()])?;
    check_payload(
        bundle,
        &Payload {
            target: selected.target.clone(),
            binary: selected.binary.clone(),
            size: selected.size,
            sha256: selected.sha256.clone(),
        },
    )?;
    Ok(selected)
}
/// Validate a directory whose manifest lists exactly one payload.
pub fn verify(bundle: &Path) -> Result<Manifest> {
    let release = verify_release(bundle)?;
    match release.views().as_slice() {
        [only] => Ok(only.clone()),
        _ => Err("bundle lists several payloads; select a target".into()),
    }
}
/// Check a `SHA256SUMS` body against `expected` `(asset name, sha256)` pairs.
/// Each must appear exactly once with the expected hash; other well-formed
/// entries are allowed, malformed lines are not.
pub fn verify_sums(sums: &str, expected: &[(&str, &str)]) -> Result<()> {
    let mut entries: Vec<(&str, &str)> = Vec::new();
    for line in sums.lines() {
        let (hash, name) = line.split_once("  ").ok_or("malformed checksum line")?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !component(name)
        {
            return Err("malformed checksum line".into());
        }
        entries.push((hash, name));
    }
    for (name, sha) in expected {
        let mut found = entries.iter().filter(|(_, n)| n == name);
        match (found.next(), found.next()) {
            (Some((hash, _)), None) if hash == sha => {}
            (Some(_), None) => return Err("checksum mismatch".into()),
            _ => return Err("checksum entry missing or duplicated".into()),
        }
    }
    Ok(())
}
fn write_manifest(output: &Path, release: &ReleaseManifest) -> Result<()> {
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output.join("release-manifest.json"))?;
    out.write_all(&serde_json::to_vec_pretty(release)?)?;
    out.sync_all()?;
    Ok(())
}
/// Write a newly built binary to a NEW bundle directory. No existing version is overwritten.
pub fn package(binary: &Path, output: &Path, mut manifest: Manifest) -> Result<Manifest> {
    let (size, sha) = digest(binary)?;
    manifest.schema_version = 2;
    manifest.size = size;
    manifest.sha256 = sha;
    manifest.validate()?;
    let release = ReleaseManifest {
        schema_version: 2,
        profile: manifest.profile.clone(),
        product: manifest.product.clone(),
        version: manifest.version.clone(),
        source_commit: manifest.source_commit.clone(),
        state_schema: manifest.state_schema,
        run_id: manifest.run_id,
        run_attempt: manifest.run_attempt,
        payloads: vec![Payload {
            target: manifest.target.clone(),
            binary: manifest.binary.clone(),
            size,
            sha256: manifest.sha256.clone(),
        }],
    };
    release.validate()?;
    fs::create_dir(output)?;
    fs::copy(binary, output.join(&manifest.binary))?;
    write_manifest(output, &release)?;
    verify(output)
}
/// Combine verified bundles of one release (same identity, distinct targets)
/// into a NEW directory with a single manifest listing every payload.
pub fn merge(bundles: &[PathBuf], output: &Path) -> Result<ReleaseManifest> {
    let mut merged: Option<ReleaseManifest> = None;
    let mut sources: Vec<(PathBuf, Payload)> = Vec::new();
    for bundle in bundles {
        let release = verify_release(bundle)?;
        match &mut merged {
            None => {
                merged = Some(ReleaseManifest {
                    schema_version: 2,
                    ..release.clone()
                })
            }
            Some(first) => {
                if (
                    &first.profile,
                    &first.product,
                    &first.version,
                    &first.source_commit,
                ) != (
                    &release.profile,
                    &release.product,
                    &release.version,
                    &release.source_commit,
                ) || first.state_schema != release.state_schema
                    || first.run_id != release.run_id
                    || first.run_attempt != release.run_attempt
                {
                    return Err("bundles belong to different releases".into());
                }
                first.payloads.extend(release.payloads.clone());
            }
        }
        sources.extend(release.payloads.into_iter().map(|p| (bundle.clone(), p)));
    }
    let mut merged = merged.ok_or("no bundles to merge")?;
    merged.payloads.sort_by(|a, b| a.target.cmp(&b.target));
    merged.validate()?;
    fs::create_dir(output)?;
    for (bundle, payload) in &sources {
        fs::copy(bundle.join(&payload.binary), output.join(&payload.binary))?;
    }
    write_manifest(output, &merged)?;
    verify_release(output)
}
fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if !m.is_dir() => return Err("installation path is a link or non-directory".into()),
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path)?,
        Err(e) => return Err(e.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn layout(home: &Path) -> Result<(PathBuf, File)> {
    if !home.is_absolute() {
        return Err("installation home must be absolute".into());
    }
    private_dir(home)?;
    let base = home.join("standalone");
    private_dir(&base)?;
    private_dir(&base.join("releases"))?;
    let lock_path = base.join("install.lock");
    if fs::symlink_metadata(&lock_path).is_ok() {
        regular(&lock_path)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    FileExt::lock_exclusive(&file)?;
    Ok((base, file))
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
#[cfg(unix)]
fn activate(base: &Path, manifest: &Manifest, bin_dir: &Path) -> Result<()> {
    use std::os::unix::{fs::PermissionsExt, fs::symlink};
    if !bin_dir.is_absolute() {
        return Err("bin directory must be absolute".into());
    }
    if !bin_dir.is_dir() {
        return Err("create the bin directory explicitly before installation".into());
    }
    let launcher = bin_dir.join(&manifest.product);
    let marker = format!("#!/bin/sh\n# {} managed launcher v1\n", manifest.product);
    if fs::symlink_metadata(&launcher).is_ok() {
        regular(&launcher)?;
        if fs::metadata(&launcher)?.len() > 8192
            || !fs::read_to_string(&launcher)?.starts_with(&marker)
        {
            return Err("refusing to replace an unmanaged launcher".into());
        }
    }
    let current = base.join("current");
    if let Ok(m) = fs::symlink_metadata(&current) {
        if !m.file_type().is_symlink() {
            return Err("current must be a managed symlink".into());
        }
        let previous = fs::read_link(&current)?;
        if previous.is_absolute()
            || !previous.starts_with("releases")
            || previous.components().count() != 2
        {
            return Err("unmanaged current target".into());
        }
        let old = verify_target(&base.join(previous), &manifest.target)?;
        if old.product != manifest.product || old.target != manifest.target {
            return Err("incompatible previous installation".into());
        }
        // Never run an older state schema against newer on-disk state; same or
        // newer is fine (newer code reads or migrates the older state).
        if manifest.state_schema < old.state_schema {
            return Err("release predates the state schema already in use".into());
        }
    }
    // Preserve executable identity for every new process; it does not repeatedly resolve current.
    let body = format!(
        "{marker}set -eu\nbase={}\nrelease=$(readlink \"$base/current\")\nexec \"$base/$release/{}\" \"$@\"\n",
        shell_quote(base.to_str().ok_or("non-UTF8 installation path")?),
        manifest.binary
    );
    let temp_launcher = bin_dir.join(format!(
        ".{}.install-{}",
        manifest.product,
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_launcher)?;
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(&temp_launcher, fs::Permissions::from_mode(0o755))?;
    let next = base.join(format!(".current-{}", std::process::id()));
    symlink(Path::new("releases").join(&manifest.version), &next)?;
    // Both versions use the same target-specific asset name. Existing services are not restarted.
    fs::rename(&temp_launcher, &launcher)?;
    fs::rename(&next, current)?;
    Ok(())
}
#[cfg(not(unix))]
fn activate(_: &Path, _: &Manifest, _: &Path) -> Result<()> {
    Err("installer supports Unix only".into())
}
/// Install without migration, pruning, stopping services or changing host configuration.
pub fn install(bundle: &Path, home: &Path, bin_dir: &Path) -> Result<Manifest> {
    install_for(bundle, home, bin_dir, host_target()?)
}
fn install_for(bundle: &Path, home: &Path, bin_dir: &Path, target: &str) -> Result<Manifest> {
    let manifest = verify_target(bundle, target)?;
    let (base, _lock) = layout(home)?;
    let destination = base.join("releases").join(&manifest.version);
    if fs::symlink_metadata(&destination).is_ok() {
        if verify_target(&destination, target)? != manifest {
            return Err("immutable version already exists with different bytes or metadata".into());
        }
    } else {
        let stage = base.join(format!(".install-{}", std::process::id()));
        fs::create_dir(&stage)?;
        fs::copy(bundle.join(&manifest.binary), stage.join(&manifest.binary))?;
        fs::copy(
            bundle.join("release-manifest.json"),
            stage.join("release-manifest.json"),
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                stage.join(&manifest.binary),
                fs::Permissions::from_mode(0o755),
            )?;
        }
        verify_target(&stage, target)?;
        fs::rename(stage, &destination)?;
    }
    activate(&base, &manifest, bin_dir)?;
    Ok(manifest)
}
/// Activate a retained version only after verifying its integrity and state profile.
pub fn use_version(home: &Path, bin_dir: &Path, version: &str) -> Result<Manifest> {
    use_version_for(home, bin_dir, version, host_target()?)
}
fn use_version_for(home: &Path, bin_dir: &Path, version: &str, target: &str) -> Result<Manifest> {
    if !component(version) {
        return Err("invalid version component".into());
    }
    let (base, _lock) = layout(home)?;
    let manifest = verify_target(&base.join("releases").join(version), target)?;
    activate(&base, &manifest, bin_dir)?;
    Ok(manifest)
}
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Test fixtures fail explicitly"
)]
mod tests {
    use super::*;
    const DARWIN: &str = "aarch64-apple-darwin";
    const LINUX: &str = "x86_64-unknown-linux-gnu";
    fn fixture(root: &Path, version: &str, state_schema: u32) -> PathBuf {
        fixture_for(root, version, state_schema, DARWIN)
    }
    fn fixture_for(root: &Path, version: &str, state_schema: u32, target: &str) -> PathBuf {
        let binary = root.join(format!("source-{version}-{target}"));
        fs::write(&binary, format!("fixture for {target}, not executable")).unwrap();
        let bundle = root.join(format!("bundle-{version}-{target}"));
        package(
            &binary,
            &bundle,
            Manifest {
                schema_version: 1,
                profile: "single-binary-v1".into(),
                product: "agent-test".into(),
                version: version.into(),
                source_commit: "a".repeat(40),
                target: target.into(),
                binary: format!("agent-test-{target}"),
                size: 1,
                sha256: "0".repeat(64),
                state_schema,
                run_id: None,
                run_attempt: None,
            },
        )
        .unwrap();
        bundle
    }
    #[test]
    fn verified_bundle() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(
            verify(&fixture(t.path(), "0.1.0", 0)).unwrap().version,
            "0.1.0"
        );
    }
    #[test]
    fn tampered_binary_rejected() {
        let t = tempfile::tempdir().unwrap();
        let b = fixture(t.path(), "0.1.0", 0);
        fs::write(b.join(format!("agent-test-{DARWIN}")), b"tampered").unwrap();
        assert!(verify(&b).is_err());
    }
    #[test]
    fn extra_file_rejected() {
        let t = tempfile::tempdir().unwrap();
        let b = fixture(t.path(), "0.1.0", 0);
        fs::write(b.join("extra"), b"x").unwrap();
        assert!(verify(&b).is_err());
    }
    #[test]
    fn traversal_rejected() {
        for s in ["..", "../x", "/tmp/x", "x/y", "x\\y", ""] {
            assert!(!component(s));
        }
    }
    #[cfg(unix)]
    #[test]
    fn binary_symlink_rejected() {
        let t = tempfile::tempdir().unwrap();
        let b = fixture(t.path(), "0.1.0", 0);
        let p = b.join(format!("agent-test-{DARWIN}"));
        fs::remove_file(&p).unwrap();
        std::os::unix::fs::symlink(t.path().join(format!("source-0.1.0-{DARWIN}")), p).unwrap();
        assert!(verify(&b).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn install_noop_and_rollback() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("home");
        let bin = t.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let a = fixture(t.path(), "0.1.0", 0);
        let b = fixture(t.path(), "0.2.0", 0);
        install_for(&a, &h, &bin, DARWIN).unwrap();
        install_for(&a, &h, &bin, DARWIN).unwrap();
        install_for(&b, &h, &bin, DARWIN).unwrap();
        use_version_for(&h, &bin, "0.1.0", DARWIN).unwrap();
        assert_eq!(
            fs::read_link(h.join("standalone/current")).unwrap(),
            PathBuf::from("releases/0.1.0")
        );
    }
    #[cfg(unix)]
    #[test]
    fn stateful_upgrade_and_rollback_keep_owner_state_intact() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("home");
        // The owner's pre-existing local state, outside `standalone/`.
        fs::create_dir_all(h.join("state/v1")).unwrap();
        fs::write(h.join("config.toml"), b"[storage]\nroot = \"/tmp/wt\"\n").unwrap();
        fs::write(h.join("state/v1/registry.json"), b"{\"entries\":[]}\n").unwrap();
        let (config, registry) = (
            fs::read(h.join("config.toml")).unwrap(),
            fs::read(h.join("state/v1/registry.json")).unwrap(),
        );
        let bin = t.path().join("bin");
        fs::create_dir(&bin).unwrap();
        install_for(
            &fixture(t.path(), "0.1.0", CURRENT_STATE_SCHEMA),
            &h,
            &bin,
            DARWIN,
        )
        .unwrap();
        install_for(
            &fixture(t.path(), "0.2.0", CURRENT_STATE_SCHEMA),
            &h,
            &bin,
            DARWIN,
        )
        .unwrap();
        use_version_for(&h, &bin, "0.1.0", DARWIN).unwrap();
        assert_eq!(
            fs::read_link(h.join("standalone/current")).unwrap(),
            PathBuf::from("releases/0.1.0")
        );
        assert_eq!(fs::read(h.join("config.toml")).unwrap(), config);
        assert_eq!(
            fs::read(h.join("state/v1/registry.json")).unwrap(),
            registry
        );
    }
    #[cfg(unix)]
    #[test]
    fn older_state_schema_cannot_run_over_newer_state() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("home");
        let bin = t.path().join("bin");
        fs::create_dir(&bin).unwrap();
        install_for(
            &fixture(t.path(), "0.1.0", CURRENT_STATE_SCHEMA),
            &h,
            &bin,
            DARWIN,
        )
        .unwrap();
        assert!(install_for(&fixture(t.path(), "0.2.0", 0), &h, &bin, DARWIN).is_err());
        assert!(use_version_for(&h, &bin, "0.2.0", DARWIN).is_err());
        assert_eq!(
            fs::read_link(h.join("standalone/current")).unwrap(),
            PathBuf::from("releases/0.1.0")
        );
    }
    #[cfg(unix)]
    #[test]
    fn unmanaged_launcher_preserved() {
        let t = tempfile::tempdir().unwrap();
        let b = fixture(t.path(), "0.1.0", 0);
        let bin = t.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("agent-test"), b"mine").unwrap();
        assert!(install_for(&b, &t.path().join("home"), &bin, DARWIN).is_err());
        assert_eq!(fs::read(bin.join("agent-test")).unwrap(), b"mine");
    }
    fn release_of(root: &Path, version: &str) -> (PathBuf, ReleaseManifest) {
        let merged = root.join(format!("release-{version}"));
        let bundles = [
            fixture_for(root, version, 1, LINUX),
            fixture_for(root, version, 1, DARWIN),
        ];
        let release = merge(&bundles, &merged).unwrap();
        (merged, release)
    }
    /// Copy the manifest and one payload into a fresh directory, as the installer downloads them.
    fn single_download(root: &Path, release_dir: &Path, binary: &str) -> PathBuf {
        let dir = root.join(format!("download-{binary}"));
        fs::create_dir(&dir).unwrap();
        for name in ["release-manifest.json", binary] {
            fs::copy(release_dir.join(name), dir.join(name)).unwrap();
        }
        dir
    }
    fn legacy_manifest(binary: &[u8], target: &str) -> String {
        format!(
            r#"{{"schema_version":1,"profile":"single-binary-v1","product":"agent-test","version":"0.1.4","source_commit":"{}","target":"{target}","binary":"agent-test-{target}","size":{},"sha256":"{:x}","state_schema":1,"run_id":7,"run_attempt":1}}"#,
            "b".repeat(40),
            binary.len(),
            Sha256::digest(binary)
        )
    }
    #[test]
    fn legacy_single_payload_manifest_is_still_read() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("old");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(format!("agent-test-{DARWIN}")), b"old bytes").unwrap();
        fs::write(
            dir.join("release-manifest.json"),
            legacy_manifest(b"old bytes", DARWIN),
        )
        .unwrap();
        let release = verify_release(&dir).unwrap();
        assert_eq!((release.schema_version, release.payloads.len()), (1, 1));
        let m = verify(&dir).unwrap();
        assert_eq!((m.target.as_str(), m.run_id), (DARWIN, Some(7)));
        assert_eq!(verify_target(&dir, DARWIN).unwrap(), m);
        assert!(verify_target(&dir, LINUX).is_err());
        // An old release still installs and rolls back like a new one.
        let (h, bin) = (t.path().join("home"), t.path().join("bin"));
        fs::create_dir(&bin).unwrap();
        install_for(&dir, &h, &bin, DARWIN).unwrap();
        use_version_for(&h, &bin, "0.1.4", DARWIN).unwrap();
    }
    #[test]
    fn legacy_manifest_rejects_unknown_fields_and_payload_lists() {
        let good = legacy_manifest(b"x", DARWIN);
        assert!(ReleaseManifest::parse(good.as_bytes()).is_ok());
        let extra = good.replacen('{', r#"{"extra":1,"#, 1);
        assert!(ReleaseManifest::parse(extra.as_bytes()).is_err());
        let list = good.replacen('{', r#"{"payloads":[],"#, 1);
        assert!(ReleaseManifest::parse(list.as_bytes()).is_err());
        let v3 = good.replacen("\"schema_version\":1", "\"schema_version\":3", 1);
        assert!(ReleaseManifest::parse(v3.as_bytes()).is_err());
    }
    #[test]
    fn merged_release_lists_every_payload_and_verifies() {
        let t = tempfile::tempdir().unwrap();
        let (dir, release) = release_of(t.path(), "0.2.0");
        assert_eq!(release.schema_version, 2);
        let targets: Vec<_> = release.payloads.iter().map(|p| p.target.as_str()).collect();
        assert_eq!(targets, [DARWIN, LINUX]);
        assert_eq!(verify_release(&dir).unwrap(), release);
        assert_eq!(
            ReleaseManifest::parse(&fs::read(dir.join("release-manifest.json")).unwrap()).unwrap(),
            release
        );
        // The bundle lists several payloads, so the single-payload check refuses to pick.
        assert!(verify(&dir).is_err());
    }
    #[test]
    fn target_selection_picks_the_host_payload_only() {
        let t = tempfile::tempdir().unwrap();
        let (_, release) = release_of(t.path(), "0.2.0");
        for target in [DARWIN, LINUX] {
            let m = release.select(target).unwrap();
            assert_eq!(m.target, target);
            assert_eq!(m.binary, format!("agent-test-{target}"));
        }
    }
    #[test]
    fn unknown_or_unlisted_targets_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let (_, release) = release_of(t.path(), "0.2.0");
        for target in [
            "",
            "riscv64gc-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
            "../x",
        ] {
            assert!(release.select(target).is_err(), "{target}");
        }
        // A supported target that this release does not ship is refused, too.
        let mut darwin_only = release.clone();
        darwin_only.payloads.retain(|p| p.target == DARWIN);
        assert!(darwin_only.select(LINUX).is_err());
        // A listed but unsupported target is never selectable.
        let mut exotic = release;
        exotic.payloads[0].target = "riscv64gc-unknown-linux-gnu".into();
        exotic.payloads[0].binary = "agent-test-riscv64gc-unknown-linux-gnu".into();
        assert!(exotic.validate().is_ok());
        assert!(exotic.select("riscv64gc-unknown-linux-gnu").is_err());
    }
    #[test]
    fn invalid_multi_payload_manifests_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let (_, good) = release_of(t.path(), "0.2.0");
        let mut duplicate = good.clone();
        duplicate.payloads[1] = duplicate.payloads[0].clone();
        let mut empty = good.clone();
        empty.payloads.clear();
        let mut renamed = good.clone();
        renamed.payloads[0].binary = "agent-test-other".into();
        let mut escaping = good.clone();
        escaping.payloads[0].binary = "../agent-test".into();
        let mut crowded = good.clone();
        crowded.payloads = (0..=MAX_PAYLOADS)
            .map(|i| Payload {
                target: format!("target-{i}"),
                binary: format!("agent-test-target-{i}"),
                ..good.payloads[0].clone()
            })
            .collect();
        let mut v1_list = good.clone();
        v1_list.schema_version = 1;
        for bad in [duplicate, empty, renamed, escaping, crowded, v1_list] {
            assert!(bad.validate().is_err());
        }
    }
    #[test]
    fn merge_refuses_mixed_releases_and_duplicate_targets() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (
            fixture_for(t.path(), "0.2.0", 1, DARWIN),
            fixture_for(t.path(), "0.2.1", 1, LINUX),
        );
        assert!(merge(&[a.clone(), b], &t.path().join("mixed")).is_err());
        assert!(merge(&[a.clone(), a], &t.path().join("twice")).is_err());
        assert!(merge(&[], &t.path().join("none")).is_err());
    }
    #[test]
    fn payload_verification_failures_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let (dir, _) = release_of(t.path(), "0.2.0");
        let linux = format!("agent-test-{LINUX}");
        let download = single_download(t.path(), &dir, &linux);
        let m = verify_target(&download, LINUX).unwrap();
        assert_eq!(m.binary, linux);
        // The other target is listed but its bytes are absent: not selectable here.
        assert!(verify_target(&download, DARWIN).is_err());
        // Full verification needs every payload.
        assert!(verify_release(&download).is_err());
        // A foreign payload next to the selected one breaks the inventory.
        fs::copy(
            dir.join(format!("agent-test-{DARWIN}")),
            download.join(format!("agent-test-{DARWIN}")),
        )
        .unwrap();
        assert!(verify_target(&download, LINUX).is_err());
        fs::remove_file(download.join(format!("agent-test-{DARWIN}"))).unwrap();
        // Tampered bytes of the selected payload, in a partial and in the full bundle.
        fs::write(dir.join(&linux), b"tampered").unwrap();
        assert!(verify_release(&dir).is_err());
        fs::write(download.join(&linux), b"tampered").unwrap();
        assert!(verify_target(&download, LINUX).is_err());
        // A same-length substitution is caught by the hash, not the size.
        let original = fs::read(dir.join(format!("agent-test-{DARWIN}"))).unwrap();
        let mut flipped = original.clone();
        flipped[0] ^= 1;
        fs::write(dir.join(format!("agent-test-{DARWIN}")), flipped).unwrap();
        assert!(verify_release(&dir).is_err());
    }
    #[test]
    fn manifest_describing_another_hosts_binary_cannot_be_installed() {
        let t = tempfile::tempdir().unwrap();
        let (dir, _) = release_of(t.path(), "0.2.0");
        let download = single_download(t.path(), &dir, &format!("agent-test-{DARWIN}"));
        let (h, bin) = (t.path().join("home"), t.path().join("bin"));
        fs::create_dir(&bin).unwrap();
        assert!(install_for(&download, &h, &bin, LINUX).is_err());
        assert!(install_for(&download, &h, &bin, "riscv64gc-unknown-linux-gnu").is_err());
        assert!(!bin.join("agent-test").exists());
        let m = install_for(&download, &h, &bin, DARWIN).unwrap();
        assert_eq!(m.target, DARWIN);
        // The installed directory keeps the whole manifest but only the host payload.
        let installed = h.join("standalone/releases/0.2.0");
        assert_eq!(verify_target(&installed, DARWIN).unwrap(), m);
        assert!(verify_target(&installed, LINUX).is_err());
    }
    #[test]
    fn checksum_file_must_cover_every_payload() {
        let sha = |c: char| c.to_string().repeat(64);
        let sums = format!(
            "{}  agent-x-a\n{}  release-manifest.json\n{}  install.sh\n",
            sha('a'),
            sha('b'),
            sha('c')
        );
        let want = [("agent-x-a", sha('a')), ("release-manifest.json", sha('b'))];
        let expected: Vec<_> = want.iter().map(|(n, h)| (*n, h.as_str())).collect();
        assert!(verify_sums(&sums, &expected).is_ok());
        let (a, d) = (sha('a'), sha('d'));
        let missing = [("agent-x-b", a.as_str())];
        assert!(verify_sums(&sums, &missing).is_err());
        let wrong = [("agent-x-a", d.as_str())];
        assert!(verify_sums(&sums, &wrong).is_err());
        let duplicated = format!("{sums}{}  agent-x-a\n", sha('a'));
        assert!(verify_sums(&duplicated, &expected).is_err());
        for malformed in [
            "short  agent-x-a\n",
            &format!("{} agent-x-a\n", sha('a')),
            &format!("{}  ../x\n", sha('a')),
            &format!("{}  agent-x-a\n", sha('A')),
        ] {
            assert!(verify_sums(malformed, &expected).is_err(), "{malformed}");
        }
    }
    #[test]
    fn host_target_is_a_supported_target() {
        // CI and development hosts are all supported ones; an unsupported host is refused, not guessed.
        assert!(SUPPORTED_TARGETS.contains(&host_target().unwrap()));
    }
}
