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
    if let Some(path) = find_cached(&cache_dir, track_id, client.quality()) {
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
    let (target, partial) = cache_paths(&cache_dir, track_id, info.extension(), client.quality());

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
    let fetch = async {
        let response = client
            .http_get(url)
            .header(reqwest::header::RANGE, format!("bytes={offset}-"))
            .send()
            .await
            .context("cannot request the file tail")?;

        // A server that ignores Range answers 200 OK with the whole file, and
        // storing that at the tail offset would write the file's beginning over
        // the tail region. The tail is an optimization, so distrusting the
        // response must fall back to the sequential download instead.
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            anyhow::bail!("the server ignored the range request");
        }

        if let Some(range) = response.headers().get(reqwest::header::CONTENT_RANGE) {
            let starts_at_offset = range
                .to_str()
                .ok()
                .and_then(|value| value.strip_prefix("bytes "))
                .and_then(|value| value.split('-').next())
                .and_then(|value| value.parse::<u64>().ok())
                == Some(offset);
            if !starts_at_offset {
                anyhow::bail!("the server answered with a different range");
            }
        }

        response
            .bytes()
            .await
            .context("the file tail did not arrive in full")
            .map(|bytes| bytes.to_vec())
    };
    // The tail is small, so unlike the track body it gets a total timeout.
    tokio::time::timeout(std::time::Duration::from_secs(30), fetch)
        .await
        .context("the file tail request timed out")?
}

/// Downloads a track into the cache in full and returns its path.
///
/// Used to fetch the next track ahead of time, where latency does not matter
/// and having the finished file on disk does.
pub async fn ensure_track(client: &Client, cache_dir: &Path, track_id: &str) -> Result<PathBuf> {
    if let Some(existing) = find_cached(cache_dir, track_id, client.quality()) {
        return Ok(existing);
    }

    tokio::fs::create_dir_all(cache_dir)
        .await
        .with_context(|| format!("cannot create the cache at {}", cache_dir.display()))?;

    let info = client.download_info(track_id).await?;
    let (target, partial) = cache_paths(cache_dir, track_id, info.extension(), client.quality());

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
fn cache_paths(
    cache_dir: &Path,
    track_id: &str,
    extension: &str,
    quality: &str,
) -> (PathBuf, PathBuf) {
    (
        cache_dir.join(format!("{track_id}.{quality}.{extension}")),
        cache_dir.join(format!("{track_id}.{quality}.part")),
    )
}

async fn save_response(
    mut response: reqwest::Response,
    target: &Path,
    partial: &Path,
) -> Result<()> {
    let result = try_save_response(&mut response, target, partial).await;
    // A failed download never resumes, so the leftover fragment is useless.
    if result.is_err() {
        let _ = tokio::fs::remove_file(partial).await;
    }
    result
}

async fn try_save_response(
    response: &mut reqwest::Response,
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

fn find_cached(cache_dir: &Path, track_id: &str, quality: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(cache_dir).ok()?;
    let prefix = format!("{track_id}.{quality}.");
    let mut legacy = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let extension = path.extension().and_then(|ext| ext.to_str());
        if extension == Some("part") || !entry.metadata().is_ok_and(|m| m.is_file() && m.len() > 0)
        {
            continue;
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&prefix))
        {
            return Some(path);
        }
        // Older releases stored tracks without a quality marker. Reuse only
        // formats that are unambiguous for the selected quality.
        if path.file_stem().and_then(|stem| stem.to_str()) == Some(track_id)
            && matches!(
                (quality, extension),
                ("high", Some("mp3")) | ("lossless", Some("mp4" | "flac"))
            )
        {
            legacy = Some(path);
        }
    }
    legacy
}

/// How old a `.part` file must be before `prune` treats it as abandoned.
const PART_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

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
        if path.extension().is_some_and(|ext| ext == "part") {
            // Young fragments belong to this session's in-progress downloads;
            // only ones abandoned by a crash or kill are old enough to drop.
            if meta
                .modified()
                .is_ok_and(|m| m.elapsed().is_ok_and(|age| age > PART_MAX_AGE))
            {
                let _ = std::fs::remove_file(&path);
            }
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
        if keep.is_some_and(|id| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&format!("{id}.")))
        }) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_quality_uses_its_own_cache_and_reuses_compatible_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("42.high.mp3"), b"high").unwrap();
        std::fs::write(root.join("42.mp3"), b"legacy").unwrap();
        std::fs::write(root.join("42.lossless.part"), b"unfinished").unwrap();
        assert_eq!(
            find_cached(root, "42", "high"),
            Some(root.join("42.high.mp3"))
        );
        assert!(find_cached(root, "42", "lossless").is_none());
        std::fs::write(root.join("42.mp4"), b"legacy-lossless").unwrap();
        assert_eq!(
            find_cached(root, "42", "lossless"),
            Some(root.join("42.mp4"))
        );
        std::fs::write(root.join("42.lossless.mp4"), b"lossless").unwrap();
        assert_eq!(
            find_cached(root, "42", "lossless"),
            Some(root.join("42.lossless.mp4"))
        );
        let (_, high_partial) = cache_paths(root, "42", "mp3", "high");
        let (_, lossless_partial) = cache_paths(root, "42", "mp4", "lossless");
        assert_ne!(high_partial, lossless_partial);
    }

    #[test]
    fn trimming_keeps_the_playing_track_and_in_progress_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for filename in [
            "42.high.mp3",
            "42.lossless.mp4",
            "42.mp3",
            "43.high.mp3",
            "421.high.mp3",
            "43.lossless.part",
        ] {
            std::fs::write(root.join(filename), b"content").unwrap();
        }
        prune(root, 0, Some("42")).unwrap();
        for filename in [
            "42.high.mp3",
            "42.lossless.mp4",
            "42.mp3",
            "43.lossless.part",
        ] {
            assert!(root.join(filename).exists());
        }
        for filename in ["43.high.mp3", "421.high.mp3"] {
            assert!(!root.join(filename).exists());
        }
    }

    #[test]
    fn trimming_removes_only_abandoned_part_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let stale = root.join("43.lossless.part");
        std::fs::write(&stale, b"fragment").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - PART_MAX_AGE * 2)
            .unwrap();
        let fresh = root.join("44.lossless.part");
        std::fs::write(&fresh, b"fragment").unwrap();
        prune(root, u64::MAX, None).unwrap();
        assert!(!stale.exists());
        assert!(fresh.exists());
    }
}
