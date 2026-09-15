use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const LATEST_RELEASE: &str = "https://api.github.com/repos/ijustbsd/tuiya/releases/latest";
const MAX_BINARY_SIZE: u64 = 256 * 1024 * 1024;

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

pub async fn run() -> Result<()> {
    let target = release_target(std::env::consts::OS, std::env::consts::ARCH)?;
    let current = env!("TUIYA_VERSION");
    let http = reqwest::Client::builder()
        .user_agent(concat!("tuiya/", env!("TUIYA_VERSION")))
        .https_only(true)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()?;

    println!("Checking for updates (installed: {current})...");
    let response = http
        .get(LATEST_RELEASE)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("cannot reach GitHub releases")?;
    match response.status() {
        reqwest::StatusCode::NOT_FOUND => bail!("No published tuiya release found on GitHub"),
        reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::TOO_MANY_REQUESTS => {
            bail!("GitHub refused the update check (possibly an API rate limit); try again later")
        }
        _ => {}
    }
    let release: Release = response
        .error_for_status()
        .context("GitHub release lookup failed")?
        .json()
        .await
        .context("cannot read the GitHub release metadata")?;
    let latest = Version::parse(release.tag_name.trim_start_matches('v'))
        .context("GitHub release has an invalid version")?;
    if !needs_update(current, &latest) {
        println!("tuiya {current} is already up to date (latest release: {latest}).");
        return Ok(());
    }
    let name = format!("tuiya-{latest}-{target}.tar.gz");
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .with_context(|| {
            format!("Release {latest} has no {name}; try again after its builds finish")
        })?;

    let executable = std::env::current_exe()
        .context("cannot locate the running tuiya binary")?
        .canonicalize()
        .context("cannot resolve the installed tuiya path")?;
    let parent = executable
        .parent()
        .context("binary has no parent directory")?;
    // The temporary directory is beside the executable so rename stays atomic.
    let staging = tempfile::Builder::new()
        .prefix(".tuiya-update-")
        .tempdir_in(parent)
        .with_context(|| {
            format!(
                "Cannot write to {}. Run with the permissions used to install tuiya, or install it in ~/.local/bin.",
                parent.display()
            )
        })?;

    println!("Downloading tuiya {latest} for {target}...");
    let archive_path = staging.path().join("release.tar.gz");
    download(&http, asset, &archive_path).await?;
    let binary = staging.path().join("tuiya");
    unpack(&archive_path, &binary)?;
    replace(&binary, &executable, &latest.to_string()).await?;
    println!(
        "Updated tuiya {current} → {latest} at {}",
        executable.display()
    );
    Ok(())
}

fn release_target(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64" | "aarch64") => Ok("universal-apple-darwin"),
        _ => bail!("Self-update is not supported on {os}/{arch}; build tuiya from source"),
    }
}

fn needs_update(current: &str, latest: &Version) -> bool {
    Version::parse(current.trim_start_matches('v'))
        .map(|current| current.cmp_precedence(latest).is_lt())
        // An untagged local build can still install the latest release.
        .unwrap_or(true)
}

async fn download(http: &reqwest::Client, asset: &Asset, path: &Path) -> Result<()> {
    let mut response = http
        .get(&asset.browser_download_url)
        .send()
        .await
        .context("cannot download the release archive")?
        .error_for_status()
        .context("release archive download failed")?;
    let mut file = File::create(path)?;
    let mut digest = Sha256::new();
    let mut size = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .context("release download interrupted")?
    {
        size += chunk.len() as u64;
        ensure!(
            size <= asset.size,
            "release download exceeds its declared size"
        );
        digest.update(&chunk);
        file.write_all(&chunk)?;
    }
    ensure!(size == asset.size, "release download is incomplete");
    if let Some(expected) = &asset.digest {
        let hex: String = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let actual = format!("sha256:{hex}");
        ensure!(&actual == expected, "release archive checksum mismatch");
    }
    Ok(())
}

fn unpack(archive_path: &Path, binary: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(GzDecoder::new(File::open(archive_path)?));
    let mut found = false;
    for entry in archive.entries().context("cannot read release archive")? {
        let entry = entry.context("invalid release archive entry")?;
        if entry.path()?.as_ref() != Path::new("tuiya") {
            continue;
        }
        ensure!(!found, "release archive contains multiple tuiya binaries");
        ensure!(
            entry.header().entry_type().is_file(),
            "tuiya is not a regular file"
        );
        let mut file = File::create_new(binary)?;
        let size = std::io::copy(&mut entry.take(MAX_BINARY_SIZE + 1), &mut file)?;
        ensure!(
            size > 0 && size <= MAX_BINARY_SIZE,
            "invalid tuiya binary size"
        );
        file.sync_all()?;
        found = true;
    }
    ensure!(found, "release archive does not contain tuiya");
    Ok(())
}

async fn replace(binary: &Path, executable: &Path, version: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(binary, fs::Permissions::from_mode(0o755))?;
    }
    // Catch incompatible loaders, damaged binaries and incorrectly packaged versions
    // before touching the existing installation. Drop kills a timed-out child.
    let mut verified = false;
    // Try the legacy flag first: old releases start the player on an unknown
    // command, whereas new releases reject unknown flags without loading config.
    for arg in ["--version", "version"] {
        let output = tokio::time::timeout(
            Duration::from_secs(15),
            tokio::process::Command::new(binary)
                .arg(arg)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("downloaded tuiya timed out during its version check")?
        .context("downloaded tuiya cannot run on this system; existing installation preserved")?;
        if output.status.success()
            && String::from_utf8_lossy(&output.stdout).trim() == format!("tuiya {version}")
        {
            verified = true;
            break;
        }
    }
    ensure!(
        verified,
        "downloaded tuiya failed its version check; existing installation preserved"
    );
    fs::rename(binary, executable).with_context(|| {
        format!(
            "cannot replace {}; existing installation preserved",
            executable.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn versions_compare_numerically_and_never_downgrade_releases() {
        let latest = Version::parse("0.2.10").unwrap();
        assert!(needs_update("0.2.9", &latest));
        assert!(needs_update("v0.2.9", &latest));
        assert!(needs_update("0.2.9-3-gabcdef", &latest));
        assert!(needs_update("abcdef-dirty", &latest));
        assert!(!needs_update("0.2.10", &latest));
        assert!(!needs_update("0.2.10+local", &latest));
        assert!(!needs_update("0.3.0", &latest));
    }

    #[test]
    fn targets_match_release_packages() {
        assert_eq!(
            release_target("linux", "x86_64").unwrap(),
            "x86_64-unknown-linux-gnu"
        );
        assert_eq!(
            release_target("linux", "aarch64").unwrap(),
            "aarch64-unknown-linux-gnu"
        );
        for arch in ["x86_64", "aarch64"] {
            assert_eq!(
                release_target("macos", arch).unwrap(),
                "universal-apple-darwin"
            );
        }
        assert!(release_target("linux", "riscv64").is_err());
        assert!(release_target("windows", "x86_64").is_err());
    }

    fn archive(path: &Path, entries: &[(&str, tar::EntryType, &[u8])]) {
        let encoder = GzEncoder::new(File::create(path).unwrap(), Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for (name, kind, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_entry_type(*kind);
            if kind.is_symlink() {
                header.set_link_name("/tmp/unrelated-file").unwrap();
            }
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn archive_extracts_only_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("archive.tar.gz");
        let binary = dir.path().join("tuiya");
        archive(
            &package,
            &[
                ("unrelated", tar::EntryType::Regular, b"ignore"),
                ("tuiya", tar::EntryType::Regular, b"binary"),
            ],
        );
        unpack(&package, &binary).unwrap();
        assert_eq!(fs::read(binary).unwrap(), b"binary");
        assert!(!dir.path().join("unrelated").exists());
    }

    #[test]
    fn invalid_archives_are_rejected() {
        let cases: Vec<Vec<(&str, tar::EntryType, &[u8])>> = vec![
            vec![],
            vec![("other", tar::EntryType::Regular, b"binary")],
            vec![("tuiya", tar::EntryType::Symlink, b"")],
            vec![("tuiya", tar::EntryType::Regular, b"")],
            vec![
                ("tuiya", tar::EntryType::Regular, b"first"),
                ("tuiya", tar::EntryType::Regular, b"second"),
            ],
        ];
        for entries in cases {
            let dir = tempfile::tempdir().unwrap();
            let package = dir.path().join("archive.tar.gz");
            archive(&package, &entries);
            assert!(unpack(&package, &dir.path().join("tuiya")).is_err());
        }
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("broken.tar.gz");
        fs::write(&package, b"not a gzip archive").unwrap();
        assert!(unpack(&package, &dir.path().join("tuiya")).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_preserves_old_binary_when_validation_fails() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("installed");
        let binary = dir.path().join("candidate");
        fs::write(&executable, b"old binary").unwrap();
        for data in [
            "broken executable",
            "#!/bin/sh\nexit 1\n",
            "#!/bin/sh\necho 'tuiya 0.1.0'\n",
        ] {
            fs::write(&binary, data).unwrap();
            assert!(replace(&binary, &executable, "0.2.0").await.is_err());
            assert_eq!(fs::read(&executable).unwrap(), b"old binary");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_updates_custom_filename_and_keeps_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("my player");
        let link = dir.path().join("tuiya");
        let binary = dir.path().join("candidate");
        fs::write(&executable, b"old binary").unwrap();
        symlink(&executable, &link).unwrap();
        let data = b"#!/bin/sh\n[ \"$1\" = version ] || exit 1\necho 'tuiya 0.2.0'\n";
        fs::write(&binary, data).unwrap();
        replace(&binary, &link.canonicalize().unwrap(), "0.2.0")
            .await
            .unwrap();
        assert_eq!(fs::read(&link).unwrap(), data);
        assert!(link.is_symlink());
        assert!(!binary.exists());
        assert_eq!(
            fs::metadata(&executable).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[tokio::test]
    async fn download_checks_status_size_and_checksum() {
        for (status, size, checksum, succeeds) in [
            (
                200,
                7,
                Some(
                    "sha256:0eb3e36bfb24dcd9bb1d1bece1531216b59539a8fde17ee80224af0653c92aa3"
                        .into(),
                ),
                true,
            ),
            (200, 7, None, true),
            (404, 7, None, false),
            (200, 8, None, false),
            (200, 6, None, false),
            (200, 7, Some("sha256:wrong".into()), false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/release.tar.gz", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(socket.read_u8().await.unwrap());
                }
                socket.write_all(format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: 7\r\nConnection: close\r\n\r\narchive"
                ).as_bytes()).await.unwrap();
            });
            let dir = tempfile::tempdir().unwrap();
            let asset = Asset {
                name: "test".into(),
                browser_download_url: url,
                size,
                digest: checksum,
            };
            let http = reqwest::Client::builder().no_proxy().build().unwrap();
            let result = download(&http, &asset, &dir.path().join("archive")).await;
            assert_eq!(result.is_ok(), succeeds, "{result:?}");
            server.await.unwrap();
        }
    }
}
