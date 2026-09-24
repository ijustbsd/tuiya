use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};

/// How much of the file's end to fetch up front, with a separate range request.
///
/// MP4 is probed from the tail: symphonia jumps past `mdat` and reads the last
/// few dozen kilobytes looking for trailing atoms. Measured against real
/// tracks that is about 80 KB, so this leaves a threefold margin while still
/// arriving quickly — the tail is on the critical path to the first sample.
const TAIL_BYTES: u64 = 256 * 1024;

/// How much must be buffered before playback may start.
///
/// Decoding runs on the audio callback thread, so a read that blocks there
/// stalls the output. This covers roughly six seconds of MP3 320 or two of
/// FLAC, and any connection fast enough to stream at all refills it faster
/// than playback drains it.
const START_BYTES: u64 = 256 * 1024;

/// How often readiness is re-checked while waiting to start.
const POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// How long playback may wait for the first bytes before giving up. The
/// download itself stays unbounded — this only caps the stall at the start.
const START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A partially downloaded track held in memory.
///
/// Two regions are valid at any moment: `[0, head)`, filled sequentially by the
/// main download, and `[tail_from, len)`, filled once by the tail request. They
/// meet when the download completes.
pub struct SharedBuffer {
    state: Mutex<BufferState>,
    ready: Condvar,
    len: u64,
}

struct BufferState {
    data: Vec<u8>,
    head: u64,
    tail_from: u64,
    tail_ready: bool,
    finished: bool,
    error: Option<String>,
}

/// What a waiting caller needs to know without holding the lock.
pub struct Progress {
    pub head: u64,
    pub tail_ready: bool,
    pub finished: bool,
    pub error: Option<String>,
}

impl SharedBuffer {
    pub fn new(len: u64) -> Arc<Self> {
        Arc::new(SharedBuffer {
            state: Mutex::new(BufferState {
                data: vec![0; len as usize],
                head: 0,
                // An empty tail region: nothing is valid at the end yet.
                tail_from: len,
                tail_ready: false,
                finished: false,
                error: None,
            }),
            ready: Condvar::new(),
            len,
        })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    /// Appends the next sequential chunk of the download.
    pub fn push(&self, bytes: &[u8]) {
        let mut state = self.lock();
        let start = state.head as usize;
        let end = (start + bytes.len()).min(state.data.len());
        if end > start {
            state.data[start..end].copy_from_slice(&bytes[..end - start]);
            state.head = end as u64;
        }
        self.ready.notify_all();
    }

    /// Stores the block fetched by the tail request.
    pub fn put_tail(&self, offset: u64, bytes: &[u8]) {
        let mut state = self.lock();
        let start = offset as usize;
        let end = (start + bytes.len()).min(state.data.len());
        if end > start {
            state.data[start..end].copy_from_slice(&bytes[..end - start]);
            state.tail_from = state.tail_from.min(offset);
        }
        state.tail_ready = true;
        self.ready.notify_all();
    }

    /// Marks the tail as unnecessary — the file is small enough that the
    /// sequential download will reach the end on its own.
    pub fn skip_tail(&self) {
        let mut state = self.lock();
        state.tail_ready = true;
        self.ready.notify_all();
    }

    pub fn finish(&self) {
        let mut state = self.lock();
        state.finished = true;
        self.ready.notify_all();
    }

    pub fn fail(&self, error: String) {
        let mut state = self.lock();
        state.error = Some(error);
        state.finished = true;
        self.ready.notify_all();
    }

    pub fn progress(&self) -> Progress {
        let state = self.lock();
        Progress {
            head: state.head,
            tail_ready: state.tail_ready,
            finished: state.finished,
            error: state.error.clone(),
        }
    }

    /// How much of the file has arrived sequentially, as a fraction.
    ///
    /// Seeking is clamped to this: a read inside the not-yet-downloaded gap
    /// would block the audio callback thread and stall playback outright.
    pub fn buffered_fraction(&self) -> f64 {
        if self.len == 0 {
            return 1.0;
        }
        let state = self.lock();
        (state.head as f64 / self.len as f64).clamp(0.0, 1.0)
    }

    /// The complete file, once every byte has arrived. Used to persist the
    /// stream into the on-disk cache so the next play needs no network.
    pub fn complete(&self) -> Option<Vec<u8>> {
        let state = self.lock();
        if state.error.is_none() && state.head >= self.len {
            Some(state.data.clone())
        } else {
            None
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BufferState> {
        self.state.lock().expect("stream buffer mutex poisoned")
    }

    /// Reads from `pos`, waiting if those bytes have not arrived yet.
    fn read_at(&self, pos: u64, out: &mut [u8]) -> io::Result<usize> {
        if pos >= self.len || out.is_empty() {
            return Ok(0);
        }

        let mut state = self.lock();
        loop {
            if let Some(error) = &state.error {
                return Err(io::Error::other(error.clone()));
            }

            // How far we can read without leaving a valid region.
            let valid_until = if pos < state.head {
                state.head
            } else if pos >= state.tail_from {
                self.len
            } else {
                0
            };

            if valid_until > pos {
                let n = out.len().min((valid_until - pos) as usize);
                let from = pos as usize;
                out[..n].copy_from_slice(&state.data[from..from + n]);
                return Ok(n);
            }

            if state.finished {
                // Nothing more is coming: a gap here means a truncated file.
                return Ok(0);
            }

            state = self
                .ready
                .wait(state)
                .expect("stream buffer mutex poisoned");
        }
    }
}

/// Blocks until enough has arrived for playback to start safely.
///
/// A stalled connection fails the buffer so readers blocked in `read_at`
/// unblock with an error instead of waiting forever.
pub async fn wait_playable(shared: &Arc<SharedBuffer>) -> Result<(), String> {
    let target = START_BYTES.min(shared.len());
    let wait = async {
        loop {
            let progress = shared.progress();
            if let Some(error) = progress.error {
                return Err(error);
            }
            if progress.finished || (progress.head >= target && progress.tail_ready) {
                return Ok(());
            }
            tokio::time::sleep(POLL).await;
        }
    };
    match tokio::time::timeout(START_TIMEOUT, wait).await {
        Ok(result) => result,
        Err(_) => {
            let error = "the track did not start downloading in time".to_string();
            shared.fail(error.clone());
            Err(error)
        }
    }
}

/// Whether a file of this size needs a separate tail request at all.
pub fn needs_tail(len: u64) -> bool {
    len > TAIL_BYTES
}

/// The byte offset the tail request should start at.
pub fn tail_offset(len: u64) -> u64 {
    len.saturating_sub(TAIL_BYTES)
}

/// A reader over a [`SharedBuffer`]: sequential reads block, seeks never do.
pub struct StreamReader {
    shared: Arc<SharedBuffer>,
    pos: u64,
}

impl StreamReader {
    pub fn new(shared: Arc<SharedBuffer>) -> Self {
        StreamReader { shared, pos: 0 }
    }
}

impl Read for StreamReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.shared.read_at(self.pos, buf)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for StreamReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        // The total length is known from Content-Length, so seeking never has
        // to wait for data — only the read that follows it might.
        let len = self.shared.len() as i64;
        let target = match from {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => len + offset,
            SeekFrom::Current(offset) => self.pos as i64 + offset,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the stream",
            ));
        }
        self.pos = (target as u64).min(self.shared.len());
        Ok(self.pos)
    }
}

/// Where the decoder gets its bytes: a finished file, or a live download.
pub enum TrackSource {
    File { reader: BufReader<File>, len: u64 },
    Stream(StreamReader),
}

impl TrackSource {
    pub fn open_file(path: &std::path::Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(TrackSource::File {
            reader: BufReader::new(file),
            len,
        })
    }

    /// The download behind a streamed source, for progress queries. `None`
    /// once the track is a finished file, where everything is seekable.
    pub fn buffer(&self) -> Option<Arc<SharedBuffer>> {
        match self {
            TrackSource::File { .. } => None,
            TrackSource::Stream(reader) => Some(Arc::clone(&reader.shared)),
        }
    }

    /// Total size, which symphonia uses to seek accurately.
    pub fn byte_len(&self) -> u64 {
        match self {
            TrackSource::File { len, .. } => *len,
            TrackSource::Stream(reader) => reader.shared.len(),
        }
    }
}

impl Read for TrackSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            TrackSource::File { reader, .. } => reader.read(buf),
            TrackSource::Stream(reader) => reader.read(buf),
        }
    }
}

impl Seek for TrackSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        match self {
            TrackSource::File { reader, .. } => reader.seek(from),
            TrackSource::Stream(reader) => reader.seek(from),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_at(buffer: &Arc<SharedBuffer>, pos: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        let n = buffer.read_at(pos, &mut out).expect("read failed");
        out.truncate(n);
        out
    }

    #[test]
    fn serves_the_sequential_head() {
        let buffer = SharedBuffer::new(100);
        buffer.push(&[1, 2, 3, 4]);
        assert_eq!(read_at(&buffer, 0, 4), vec![1, 2, 3, 4]);
        // A read may be cut short at the edge of what has arrived.
        assert_eq!(read_at(&buffer, 2, 10), vec![3, 4]);
    }

    #[test]
    fn serves_the_tail_before_the_head_reaches_it() {
        let buffer = SharedBuffer::new(100);
        buffer.push(&[1, 2]);
        buffer.put_tail(96, &[7, 8, 9, 10]);
        assert_eq!(read_at(&buffer, 96, 4), vec![7, 8, 9, 10]);
        assert_eq!(read_at(&buffer, 98, 9), vec![9, 10]);
    }

    #[test]
    fn a_gap_reads_empty_once_nothing_more_is_coming() {
        let buffer = SharedBuffer::new(100);
        buffer.push(&[1, 2]);
        buffer.put_tail(96, &[7, 8, 9, 10]);
        buffer.finish();
        // Offset 50 was never filled by either region.
        assert_eq!(read_at(&buffer, 50, 4), Vec::<u8>::new());
    }

    #[test]
    fn reading_past_the_end_stops() {
        let buffer = SharedBuffer::new(4);
        buffer.push(&[1, 2, 3, 4]);
        assert_eq!(read_at(&buffer, 4, 8), Vec::<u8>::new());
    }

    #[test]
    fn a_complete_download_is_only_offered_whole() {
        let buffer = SharedBuffer::new(4);
        buffer.push(&[1, 2]);
        assert!(buffer.complete().is_none());
        buffer.push(&[3, 4]);
        assert_eq!(buffer.complete(), Some(vec![1, 2, 3, 4]));
    }

    #[test]
    fn a_failed_download_is_never_cached() {
        let buffer = SharedBuffer::new(4);
        buffer.push(&[1, 2, 3, 4]);
        buffer.fail("connection reset".to_string());
        assert!(buffer.complete().is_none());
    }

    #[test]
    fn seeking_resolves_against_the_known_length() {
        let buffer = SharedBuffer::new(100);
        let mut reader = StreamReader::new(buffer);
        assert_eq!(reader.seek(SeekFrom::Start(10)).unwrap(), 10);
        assert_eq!(reader.seek(SeekFrom::Current(5)).unwrap(), 15);
        // The end is reachable without waiting for the download.
        assert_eq!(reader.seek(SeekFrom::End(-4)).unwrap(), 96);
        // Past the end clamps rather than failing.
        assert_eq!(reader.seek(SeekFrom::Start(500)).unwrap(), 100);
        assert!(reader.seek(SeekFrom::End(-500)).is_err());
    }

    #[test]
    fn buffered_fraction_tracks_the_head() {
        let buffer = SharedBuffer::new(100);
        assert_eq!(buffer.buffered_fraction(), 0.0);
        buffer.push(&[0; 25]);
        assert_eq!(buffer.buffered_fraction(), 0.25);
        buffer.push(&[0; 75]);
        assert_eq!(buffer.buffered_fraction(), 1.0);
    }
}
