//! Explicit release operations. Local preparation never commits, tags or pushes.
use crate::{Project, Result, capture, json, run, text};
use clap::Subcommand;
use family_delivery::{ReleaseManifest, host_target, verify_release, verify_sums};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
#[derive(Subcommand)]
pub enum Release {
    /// Show version/CHANGELOG edits; --apply performs local edits only.
    Prepare {
        version: String,
        #[arg(long)]
        apply: bool,
    },
    /// Publish an accepted CI bundle through a complete draft; never replace a release.
    Publish { directory: PathBuf },
    /// Observe one exact tag/commit and validate its run and bytes. No install or wake claim.
    Wait {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        tag: String,
        #[arg(long)]
        commit: String,
        #[arg(long, default_value_t = 1800)]
        timeout: u64,
        #[arg(long)]
        result_file: Option<PathBuf>,
    },
}
fn repo_valid(repo: &str) -> bool {
    let parts: Vec<_> = repo.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 100
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}
/// Replace the workspace version without reformatting the manifest.
fn edit_manifest_version(source: &str, old: &str, new: &str) -> Result<String> {
    let mut in_package = false;
    let mut found = false;
    let mut output = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[workspace.package]";
        }
        if in_package && trimmed.starts_with("version") {
            let (key, value) = line.split_once('=').ok_or("invalid workspace version")?;
            if key.trim() == "version" {
                let expected = format!("\"{old}\"");
                let offset = value
                    .find(&expected)
                    .ok_or("unexpected workspace version")?;
                if found
                    || !value[..offset].trim().is_empty()
                    || !value[offset + expected.len()..].trim().is_empty()
                        && !value[offset + expected.len()..]
                            .trim_start()
                            .starts_with('#')
                {
                    return Err("ambiguous workspace version".into());
                }
                output.push_str(key);
                output.push('=');
                output.push_str(&value[..offset]);
                output.push_str(&format!("\"{new}\""));
                output.push_str(&value[offset + expected.len()..]);
                found = true;
                continue;
            }
        }
        output.push_str(line);
    }
    if !found {
        return Err("workspace version missing".into());
    }
    Ok(output)
}
fn prepare(project: &Project, version: &str, apply: bool) -> Result<()> {
    let old = semver::Version::parse(&project.version)?;
    let new = semver::Version::parse(version)?;
    if new <= old || !new.build.is_empty() {
        return Err("version must increase and cannot contain build metadata".into());
    }
    if !text(&project.root, "git", &["status", "--porcelain"])?.is_empty() {
        return Err("prepare requires a clean checkout".into());
    }
    let changelog = fs::read_to_string(project.root.join("CHANGELOG.md"))?;
    if changelog.matches("## Unreleased").count() != 1 {
        return Err("expected exactly one Unreleased section".into());
    }
    println!(
        "{}",
        json!({"dry_run":!apply,"old":project.version,"new":version,"files":["Cargo.toml","Cargo.lock","CHANGELOG.md"],"remote_writes":0})
    );
    if !apply {
        return Ok(());
    }
    let manifest_path = project.root.join("Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)?;
    fs::write(
        manifest_path,
        edit_manifest_version(&manifest, &project.version, version)?,
    )?;
    fs::write(
        project.root.join("CHANGELOG.md"),
        changelog.replacen(
            "## Unreleased",
            &format!("## Unreleased\n\n## {version}"),
            1,
        ),
    )?;
    capture(
        &project.root,
        "cargo",
        &[
            "metadata",
            "--offline",
            "--no-deps",
            "--format-version",
            "1",
        ],
        120,
    )?;
    println!(
        "Review changes and Cargo.lock, run checks, commit, then create an annotated version tag explicitly."
    );
    Ok(())
}
fn publish(project: &Project, directory: PathBuf) -> Result<()> {
    let directory = fs::canonicalize(directory)?;
    let release = verify_release(&directory)?;
    // Only the payload for this host can be executed here; the others are
    // hash-verified, and were accepted by the build job of their own target.
    let m = release.select(host_target()?)?;
    let tag = format!("v{}", m.version);
    let repo = project.family["repository"]
        .as_str()
        .ok_or("repository missing")?;
    if !repo_valid(repo)
        || project.family["release"]["enabled"].as_bool() != Some(true)
        || project.family["qualification"].as_str() != Some("verified")
        || !project.family["compatibility"]["qualified_targets"]
            .as_array()
            .is_some_and(|a| {
                release
                    .payloads
                    .iter()
                    .all(|p| a.iter().any(|v| v.as_str() == Some(p.target.as_str())))
            })
        || !project.family["compatibility"]["qualified_hosts"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    {
        return Err("release is disabled or lacks reviewed native/host qualification".into());
    }
    if std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true")
        || std::env::var("GITHUB_REPOSITORY").as_deref() != Ok(repo)
        || std::env::var("GITHUB_REF_NAME").as_deref() != Ok(tag.as_str())
        || std::env::var("GITHUB_SHA").as_deref() != Ok(m.source_commit.as_str())
        || std::env::var("GITHUB_RUN_ID")
            .ok()
            .and_then(|v| v.parse().ok())
            != m.run_id
        || std::env::var("GITHUB_RUN_ATTEMPT")
            .ok()
            .and_then(|v| v.parse().ok())
            != m.run_attempt
        || m.run_id.is_none()
        || m.product != project.name
        || m.version != project.version
    {
        return Err("CI/tag/source/run identity mismatch".into());
    }
    if text(&project.root, "git", &["cat-file", "-t", &tag])? != "tag"
        || text(&project.root, "git", &["rev-parse", &format!("{tag}^{{}}")])? != m.source_commit
        || !text(&project.root, "git", &["status", "--porcelain"])?.is_empty()
    {
        return Err("release requires clean exact source and an annotated immutable tag".into());
    }
    let binary = directory.join(&m.binary);
    let binary_str = binary.to_str().ok_or("non-UTF8 binary path")?;
    if text(&project.root, binary_str, &["--version"])? != format!("{} {}", m.product, m.version) {
        return Err("packaged binary identity mismatch".into());
    }
    let incomplete: Vec<String> = serde_json::from_slice(&capture(
        &project.root,
        binary_str,
        &["contract", "readiness"],
        10,
    )?)?;
    if !incomplete.is_empty() {
        return Err("unimplemented tools block release".into());
    }
    require_tests(&project.root, &project.name, &["protocol::"])?;
    for args in [
        vec!["deny", "--locked", "check"],
        vec![
            "test",
            "--frozen",
            "-p",
            &project.name,
            "--test",
            "it",
            "protocol::",
        ],
    ] {
        if !Command::new("cargo")
            .current_dir(&project.root)
            .args(args)
            .env("MCP_TEST_BINARY", &binary)
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .status()?
            .success()
        {
            return Err("supply-chain or shipped-binary acceptance failed".into());
        }
    }
    if verify_release(&directory)? != release {
        return Err("payload changed after acceptance".into());
    }
    let changelog = fs::read_to_string(project.root.join("CHANGELOG.md"))?;
    let heading = format!("## {}", m.version);
    let section = changelog
        .lines()
        .skip_while(|l| *l != heading)
        .skip(1)
        .take_while(|l| !l.starts_with("## "))
        .collect::<Vec<_>>()
        .join("\n");
    if section.trim().is_empty() {
        return Err("release CHANGELOG section is empty".into());
    }
    let notes = project.root.join("target/release-notes.md");
    fs::write(&notes, section)?;
    let manifest = directory.join("release-manifest.json");
    let installer = project.root.join("install.sh");
    let sums_path = project.root.join("target/SHA256SUMS");
    let mut sums = String::new();
    let mut files: Vec<(&str, PathBuf)> = release
        .payloads
        .iter()
        .map(|p| (p.binary.as_str(), directory.join(&p.binary)))
        .collect();
    files.push(("release-manifest.json", manifest.clone()));
    files.push(("install.sh", installer.clone()));
    for (name, file) in &files {
        sums.push_str(&format!("{}  {name}\n", family_delivery::digest(file)?.1));
    }
    let expected: Vec<(&str, &str)> = release
        .payloads
        .iter()
        .map(|p| (p.binary.as_str(), p.sha256.as_str()))
        .collect();
    verify_sums(&sums, &expected)?;
    fs::write(&sums_path, sums)?;
    // Existing releases (including drafts) cause create to fail. No --clobber path exists.
    let mut create = vec!["release".to_owned(), "create".into(), tag.clone()];
    for (_, file) in &files {
        create.push(file.to_str().ok_or("asset path")?.to_owned());
    }
    create.push(sums_path.to_str().ok_or("checksum path")?.to_owned());
    for arg in [
        "--repo",
        repo,
        "--verify-tag",
        "--draft",
        "--title",
        &tag,
        "--notes-file",
        notes.to_str().ok_or("notes path")?,
    ] {
        create.push(arg.to_owned());
    }
    let create: Vec<&str> = create.iter().map(String::as_str).collect();
    run(&project.root, "gh", &create)?;
    let verify_dir = project
        .root
        .join(format!("target/upload-verify-{}", std::process::id()));
    fs::create_dir(&verify_dir)?;
    let mut download = vec![
        "release",
        "download",
        &tag,
        "--repo",
        repo,
        "--dir",
        verify_dir.to_str().ok_or("verify path")?,
        "--pattern",
        "release-manifest.json",
    ];
    for payload in &release.payloads {
        download.extend(["--pattern", &payload.binary]);
    }
    run(&project.root, "gh", &download)?;
    if verify_release(&verify_dir)? != release {
        return Err("uploaded payload differs; draft left unpublished".into());
    }
    let bootstrap_dir = project
        .root
        .join(format!("target/bootstrap-verify-{}", std::process::id()));
    fs::create_dir(&bootstrap_dir)?;
    run(
        &project.root,
        "gh",
        &[
            "release",
            "download",
            &tag,
            "--repo",
            repo,
            "--dir",
            bootstrap_dir.to_str().ok_or("verify path")?,
            "--pattern",
            "install.sh",
            "--pattern",
            "SHA256SUMS",
        ],
    )?;
    if fs::read(bootstrap_dir.join("install.sh"))? != fs::read(installer)?
        || fs::read(bootstrap_dir.join("SHA256SUMS"))? != fs::read(sums_path)?
    {
        return Err("bootstrap assets differ; draft left unpublished".into());
    }
    let mut args = vec!["release", "edit", &tag, "--repo", repo, "--draft=false"];
    if !semver::Version::parse(&m.version)?.pre.is_empty() {
        args.push("--prerelease");
    }
    run(&project.root, "gh", &args)?;
    println!(
        "{}",
        json!({"status":"published","tag":tag,"commit":m.source_commit,"artifact_integrity":"verified","provenance_verification":"not_performed"})
    );
    Ok(())
}
fn wait(
    project: &Project,
    repo: &str,
    tag: &str,
    commit: &str,
    timeout: u64,
    result_file: Option<PathBuf>,
) -> Result<()> {
    if !repo_valid(repo)
        || commit.len() != 40
        || !commit.bytes().all(|b| b.is_ascii_hexdigit())
        || timeout == 0
        || timeout > 86400
    {
        return Err("invalid release wait scope or deadline".into());
    }
    let version = semver::Version::parse(tag.strip_prefix('v').ok_or("tag must start with v")?)?;
    if !version.build.is_empty() {
        return Err("release build metadata is unsupported".into());
    }
    fs::create_dir_all(&project.target_dir)?;
    let dir = project
        .target_dir
        .join(format!("release-wait-{}", std::process::id()));
    fs::create_dir(&dir)?;
    let sums_dir = dir.with_file_name(format!("release-wait-sums-{}", std::process::id()));
    fs::create_dir(&sums_dir)?;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let remaining = || -> Result<u64> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            Err("release wait deadline exceeded".into())
        } else {
            Ok(left.as_secs().clamp(1, 60))
        }
    };
    loop {
        remaining()?;
        let response = capture(
            &project.root,
            "gh",
            &[
                "api",
                "--hostname",
                "github.com",
                &format!("repos/{repo}/releases/tags/{tag}"),
            ],
            remaining()?,
        );
        let data: Value = match response {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(_) => {
                std::thread::sleep(
                    Duration::from_secs(2).min(deadline.saturating_duration_since(Instant::now())),
                );
                continue;
            }
        };
        if data["draft"] != false {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let assets = data["assets"]
            .as_array()
            .ok_or("release asset inventory absent")?;
        let meta = assets
            .iter()
            .find(|a| a["name"] == "release-manifest.json")
            .ok_or("published manifest absent")?;
        if meta["size"].as_u64().is_none_or(|s| s > 16384) {
            return Err("published manifest too large".into());
        }
        let meta_id = meta["id"].as_u64().ok_or("manifest asset ID missing")?;
        let bytes = capture(
            &project.root,
            "gh",
            &[
                "api",
                "--hostname",
                "github.com",
                &format!("repos/{repo}/releases/assets/{meta_id}"),
                "-H",
                "Accept: application/octet-stream",
            ],
            remaining()?,
        )?;
        // Version-1 (single payload) and version-2 (several payloads) manifests are both accepted.
        let m = ReleaseManifest::parse(&bytes)?;
        if m.source_commit != commit || format!("v{}", m.version) != tag {
            return Err("release manifest identity mismatch".into());
        }
        let run_id = m.run_id.ok_or("release has no CI run identity")?;
        let run: Value = serde_json::from_slice(&capture(
            &project.root,
            "gh",
            &[
                "api",
                "--hostname",
                "github.com",
                &format!("repos/{repo}/actions/runs/{run_id}"),
            ],
            remaining()?,
        )?)?;
        if run["head_sha"] != commit
            || run["run_attempt"].as_u64() != m.run_attempt
            || run["event"] != "push"
            || run["path"] != ".github/workflows/release.yml"
        {
            return Err("release workflow identity mismatch".into());
        }
        if run["status"] != "completed" {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        if run["conclusion"] != "success" {
            return Err("release workflow failed".into());
        }
        let mut object = serde_json::from_slice::<Value>(&capture(
            &project.root,
            "gh",
            &[
                "api",
                "--hostname",
                "github.com",
                &format!("repos/{repo}/git/ref/tags/{tag}"),
            ],
            remaining()?,
        )?)?["object"]
            .clone();
        for _ in 0..4 {
            if object["type"] == "commit" {
                break;
            }
            if object["type"] != "tag" {
                return Err("unsupported tag target".into());
            }
            let sha = object["sha"].as_str().ok_or("tag SHA missing")?;
            object = serde_json::from_slice::<Value>(&capture(
                &project.root,
                "gh",
                &[
                    "api",
                    "--hostname",
                    "github.com",
                    &format!("repos/{repo}/git/tags/{sha}"),
                ],
                remaining()?,
            )?)?["object"]
                .clone();
        }
        if object["type"] != "commit" || object["sha"] != commit {
            return Err("tag no longer resolves to expected source".into());
        }
        for payload in &m.payloads {
            let asset = assets
                .iter()
                .find(|a| a["name"] == payload.binary)
                .ok_or("binary asset absent")?;
            if asset["size"].as_u64() != Some(payload.size) {
                return Err("asset inventory size mismatch".into());
            }
        }
        if !assets.iter().any(|a| a["name"] == "SHA256SUMS") {
            return Err("checksum asset absent".into());
        }
        let mut download = vec![
            "release",
            "download",
            tag,
            "--repo",
            repo,
            "--dir",
            dir.to_str().ok_or("wait path")?,
            "--pattern",
            "release-manifest.json",
        ];
        for payload in &m.payloads {
            download.extend(["--pattern", &payload.binary]);
        }
        capture(&project.root, "gh", &download, remaining()?)?;
        if verify_release(&dir)? != m {
            return Err("downloaded bytes differ from observed manifest".into());
        }
        capture(
            &project.root,
            "gh",
            &[
                "release",
                "download",
                tag,
                "--repo",
                repo,
                "--dir",
                sums_dir.to_str().ok_or("wait path")?,
                "--pattern",
                "SHA256SUMS",
            ],
            remaining()?,
        )?;
        let manifest_sha = family_delivery::digest(&dir.join("release-manifest.json"))?.1;
        let mut expected: Vec<(&str, &str)> = m
            .payloads
            .iter()
            .map(|p| (p.binary.as_str(), p.sha256.as_str()))
            .collect();
        expected.push(("release-manifest.json", manifest_sha.as_str()));
        verify_sums(&fs::read_to_string(sums_dir.join("SHA256SUMS"))?, &expected)?;
        let mut verified: Vec<&str> = m.payloads.iter().map(|p| p.binary.as_str()).collect();
        verified.extend(["release-manifest.json", "SHA256SUMS"]);
        let event = json!({"schema_version":1,"status":"published","repo":repo,"tag":tag,"commit":commit,"run_id":run_id,"run_attempt":m.run_attempt,"verified_assets":verified,"targets":m.payloads.iter().map(|p| p.target.as_str()).collect::<Vec<_>>(),"artifact_integrity":"verified","provenance_verification":"not_performed","installed":false,"agent_awakened":false});
        let text = serde_json::to_string(&event)?;
        if let Some(path) = result_file {
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(path)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        println!("{text}");
        return Ok(());
    }
}
pub fn execute(project: &Project, command: Release) -> Result<()> {
    match command {
        Release::Prepare { version, apply } => prepare(project, &version, apply),
        Release::Publish { directory } => publish(project, directory),
        Release::Wait {
            repo,
            tag,
            commit,
            timeout,
            result_file,
        } => wait(project, &repo, &tag, &commit, timeout, result_file),
    }
}

/// Fails unless each libtest filter selects at least one test of the `it`
/// binary, so a renamed module cannot turn acceptance into a run of zero tests.
fn require_tests(root: &Path, package: &str, filters: &[&str]) -> Result<()> {
    for filter in filters {
        let output = Command::new("cargo")
            .current_dir(root)
            .args([
                "test", "--frozen", "-p", package, "--test", "it", "--", "--list", filter,
            ])
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .output()?;
        let selected = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.ends_with(": test"))
            .count();
        if !output.status.success() || selected == 0 {
            return Err(format!("acceptance filter `{filter}` selects no tests").into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_scope() {
        assert!(repo_valid("DKotsyuba/agent-example"));
        for r in ["x", "x/y/z", "x/y?token=a", "/x", "https://github.com/x/y"] {
            assert!(!repo_valid(r));
        }
    }
}

#[cfg(test)]
mod version_edit_tests {
    use super::edit_manifest_version;

    #[test]
    fn preserves_comments_and_order() -> super::Result<()> {
        let source = "# leading\n[workspace.package]\nedition = \"2024\"\nversion = \"0.1.4\" # keep\nlicense = \"MIT\"\n[package]\nversion = \"other\"\n";
        assert_eq!(
            edit_manifest_version(source, "0.1.4", "0.1.5")?,
            source.replacen("\"0.1.4\"", "\"0.1.5\"", 1)
        );
        Ok(())
    }
}
