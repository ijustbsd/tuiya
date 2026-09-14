use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use crate::api::Client;

/// Returns the path to a track's file, downloading it if needed.
///
/// The file lands in the cache as `<id>.<extension>`. While the download is
/// running the bytes go to a `.part` file, so an interrupted download never
/// leaves behind something that looks complete but is truncated.
pub async fn ensure_track(client: &Client, cache_dir: &Path, track_id: &str) -> Result<PathBuf> {
    if let Some(existing) = find_cached(cache_dir, track_id) {
        return Ok(existing);
    }

    tokio::fs::create_dir_all(cache_dir)
        .await
        .with_context(|| format!("cannot create the cache at {}", cache_dir.display()))?;

    let info = client.download_info(track_id).await?;
    let target = cache_dir.join(format!("{track_id}.{}", info.extension()));
    let partial = cache_dir.join(format!("{track_id}.part"));

    let mut response = client
        .http_get(&info.url)
        .send()
        .await
        .context("cannot start the track download")?
        .error_for_status()
        .context("the server refused to serve the track")?;

    let mut file = tokio::fs::File::create(&partial)
        .await
        .with_context(|| format!("cannot create {}", partial.display()))?;

    while let Some(chunk) = response
        .chunk()
        .await
        .context("the track download was interrupted")?
    {
        file.write_all(&chunk).await.context("cannot write to the cache")?;
    }
    file.flush().await.context("cannot flush the cache to disk")?;
    drop(file);

    tokio::fs::rename(&partial, &target)
        .await
        .context("cannot rename the cached file")?;

    Ok(target)
}

fn find_cached(cache_dir: &Path, track_id: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(cache_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "part") {
            continue;
        }
        let matches = path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|stem| stem == track_id);
        if matches && entry.metadata().is_ok_and(|m| m.len() > 0) {
            return Some(path);
        }
    }
    None
}

/// Drops least-recently-used files until the cache fits under the limit.
/// The file that is playing right now is never removed.
pub fn prune(cache_dir: &Path, limit_bytes: u64, keep: Option<&Path>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(cache_dir) else {
        return Ok(());
    };

    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let accessed = meta.accessed().or_else(|_| meta.modified())?;
        total += meta.len();
        files.push((path, meta.len(), accessed));
    }

    if total <= limit_bytes {
        return Ok(());
    }

    files.sort_by_key(|(_, _, accessed)| *accessed);
    for (path, size, _) in files {
        if total <= limit_bytes {
            break;
        }
        if keep.is_some_and(|k| k == path) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }

    Ok(())
}
