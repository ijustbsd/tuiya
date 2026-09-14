use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use crate::api::Client;
use crate::stream::{self, SharedBuffer, StreamReader, TrackSource};

/// Opens a track for playback.
///
/// A fully cached track is opened straight from disk. Otherwise, with
/// `streaming` on, the download starts and this returns as soon as enough has
/// arrived to decode safely — typically under a second, instead of waiting out
/// the whole file. With streaming off it falls back to downloading in full.
pub async fn open_track(
    client: Arc<Client>,
    cache_dir: PathBuf,
    track_id: &str,
    streaming: bool,
) -> Result<TrackSource> {
    if let Some(path) = find_cached(&cache_dir, track_id) {
        return TrackSource::open_file(&path)
            .with_context(|| format!("cannot open {}", path.display()));
    }

    if !streaming {
        let path = ensure_track(&client, &cache_dir, track_id).await?;
        return TrackSource::open_file(&path)
            .with_context(|| format!("cannot open {}", path.display()));
    }

    stream_track(client, cache_dir, track_id).await
}

/// Starts a streaming download and returns a reader over it.
async fn stream_track(
    client: Arc<Client>,
    cache_dir: PathBuf,
    track_id: &str,
) -> Result<TrackSource> {
    let info = client.download_info(track_id).await?;
    let (target, partial) = cache_paths(&cache_dir, track_id, info.extension());

    let response = client
        .http_get(&info.url)
        .send()
        .await
        .context("cannot start the track download")?
        .error_for_status()
        .context("the server refused to serve the track")?;

    // Without a length the decoder cannot seek and the tail cannot be fetched,
    // so there is nothing to gain from streaming — take the simple path.
    let Some(len) = response.content_length() else {
        tokio::fs::create_dir_all(&cache_dir)
            .await
            .with_context(|| format!("cannot create the cache at {}", cache_dir.display()))?;
        save_response(response, &target, &partial).await?;
        return TrackSource::open_file(&target)
            .with_context(|| format!("cannot open {}", target.display()));
    };

    let shared = SharedBuffer::new(len);

    // The tail goes in its own range request: MP4 is probed from the end, and
    // the sequential download would not get there for another dozen seconds.
    // MP3 never reads there, and waiting for a tail it does not need would add
    // a second to every track.
    if info.probes_tail() && stream::needs_tail(len) {
        let tail_shared = Arc::clone(&shared);
        let tail_client = Arc::clone(&client);
        let url = info.url.clone();
        let offset = stream::tail_offset(len);
        tokio::spawn(async move {
            match fetch_tail(&tail_client, &url, offset).await {
                Ok(bytes) => tail_shared.put_tail(offset, &bytes),
                // A missing tail is not fatal: reads there fall back to waiting
                // for the sequential download to reach the end.
                Err(_) => tail_shared.skip_tail(),
            }
        });
    } else {
        shared.skip_tail();
    }

    let body_shared = Arc::clone(&shared);
    tokio::spawn(async move {
        let mut response = response;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => body_shared.push(&chunk),
                Ok(None) => break,
                Err(e) => {
                    body_shared.fail(format!("the track download was interrupted: {e}"));
                    return;
                }
            }
        }
        body_shared.finish();

        // Keep the finished stream so replaying it needs no network.
        if let Some(data) = body_shared.complete() {
            let _ = persist(&data, &target, &partial);
        }
    });

    stream::wait_playable(&shared)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(TrackSource::Stream(StreamReader::new(shared)))
}

async fn fetch_tail(client: &Client, url: &str, offset: u64) -> Result<Vec<u8>> {
    let response = client
        .http_get(url)
        .header(reqwest::header::RANGE, format!("bytes={offset}-"))
        .send()
        .await
        .context("cannot request the file tail")?
        .error_for_status()
        .context("the server refused a range request")?;

    Ok(response
        .bytes()
        .await
        .context("the file tail did not arrive in full")?
        .to_vec())
}

/// Downloads a track into the cache in full and returns its path.
///
/// Used to fetch the next track ahead of time, where latency does not matter
/// and having the finished file on disk does.
pub async fn ensure_track(client: &Client, cache_dir: &Path, track_id: &str) -> Result<PathBuf> {
    if let Some(existing) = find_cached(cache_dir, track_id) {
        return Ok(existing);
    }

    tokio::fs::create_dir_all(cache_dir)
        .await
        .with_context(|| format!("cannot create the cache at {}", cache_dir.display()))?;

    let info = client.download_info(track_id).await?;
    let (target, partial) = cache_paths(cache_dir, track_id, info.extension());

    let response = client
        .http_get(&info.url)
        .send()
        .await
        .context("cannot start the track download")?
        .error_for_status()
        .context("the server refused to serve the track")?;

    save_response(response, &target, &partial).await?;
    Ok(target)
}

/// The final name and the `.part` name a download writes to first, so an
/// interrupted transfer never leaves behind something that looks complete.
fn cache_paths(cache_dir: &Path, track_id: &str, extension: &str) -> (PathBuf, PathBuf) {
    (
        cache_dir.join(format!("{track_id}.{extension}")),
        cache_dir.join(format!("{track_id}.part")),
    )
}

async fn save_response(
    mut response: reqwest::Response,
    target: &Path,
    partial: &Path,
) -> Result<()> {
    let mut file = tokio::fs::File::create(partial)
        .await
        .with_context(|| format!("cannot create {}", partial.display()))?;

    while let Some(chunk) = response
        .chunk()
        .await
        .context("the track download was interrupted")?
    {
        file.write_all(&chunk)
            .await
            .context("cannot write to the cache")?;
    }
    file.flush()
        .await
        .context("cannot flush the cache to disk")?;
    drop(file);

    tokio::fs::rename(partial, target)
        .await
        .context("cannot rename the cached file")?;
    Ok(())
}

fn persist(data: &[u8], target: &Path, partial: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(partial, data)?;
    std::fs::rename(partial, target)?;
    Ok(())
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
/// The track playing right now is never removed.
pub fn prune(cache_dir: &Path, limit_bytes: u64, keep: Option<&str>) -> Result<()> {
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
        let stem = path.file_stem().and_then(|s| s.to_str());
        if keep.is_some() && stem == keep {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }

    Ok(())
}
