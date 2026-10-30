//! Audio read from a WAV file on disk while it plays, through a block cache.
//!
//! The audio thread only reads the cache (atomics, no locks, no disk). A prefetch thread keeps the
//! blocks around each reading position loaded: a few seconds ahead of it, so a slow disk has time.
//! A block that is not loaded in time reads as silence and is counted in [`DiskStream::misses`].

use liviwave_audio_io::WavReader;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Frames in a cache block (about a third of a second at 48 kHz).
pub const BLOCK: usize = 16_384;
/// Blocks kept loaded ahead of a reading position (about two seconds).
const AHEAD: usize = 6;
/// Reading positions followed at once (two clips using the file at different places).
const CURSORS: usize = 2;
/// Blocks cached: each cursor's block, those ahead of it and one behind.
const SLOTS: usize = CURSORS * (AHEAD + 2);
/// Marks an empty slot or cursor.
const NONE: u64 = u64::MAX;
/// How long the prefetch thread sleeps when everything wanted is loaded.
const IDLE: Duration = Duration::from_millis(5);

struct Slot {
    /// The block held, or `NONE`. Set last when a block is stored and first when it is replaced.
    block: AtomicU64,
    /// Planar samples as f32 bits: channel `c`, frame `i` of the block at `c * BLOCK + i`.
    /// Allocated by the prefetch thread when the slot is first filled, so a file nobody plays
    /// holds no sample memory and the audio thread never allocates.
    data: OnceLock<Box<[AtomicU32]>>,
}

struct Cache {
    channels: usize,
    frames: u64,
    sample_rate: u32,
    slots: Vec<Slot>,
    cursors: [AtomicU64; CURSORS],
    next_cursor: AtomicUsize,
    misses: AtomicU64,
    failed: AtomicBool,
    stop: AtomicBool,
}

impl Cache {
    fn holds(&self, block: u64) -> bool {
        self.slots.iter().any(|s| s.block.load(Ordering::Acquire) == block)
    }

    /// Note that `block` is being read, so the prefetch thread loads around it.
    fn touch(&self, block: u64) {
        let near = |c: u64| c != NONE && block >= c.saturating_sub(1) && block <= c.saturating_add(AHEAD as u64);
        if let Some(c) = self.cursors.iter().find(|c| near(c.load(Ordering::Relaxed))) {
            if c.load(Ordering::Relaxed) != block {
                c.store(block, Ordering::Relaxed);
            }
            return;
        }
        let free = self.cursors.iter().find(|c| c.load(Ordering::Relaxed) == NONE);
        let taken = self.next_cursor.fetch_add(1, Ordering::Relaxed) % CURSORS;
        if let Some(c) = free.or_else(|| self.cursors.get(taken)) {
            c.store(block, Ordering::Relaxed);
        }
    }

    /// Copy `out.len()` frames of channel `ch` from `offset` in `block`; `false` when the block is
    /// not loaded (or was replaced while it was read).
    fn copy(&self, block: u64, ch: usize, offset: usize, out: &mut [f32]) -> bool {
        let Some(slot) = self.slots.iter().find(|s| s.block.load(Ordering::Acquire) == block) else { return false };
        let from = ch * BLOCK + offset;
        let Some(src) = slot.data.get().and_then(|d| d.get(from..from + out.len())) else { return false };
        for (o, s) in out.iter_mut().zip(src) {
            *o = f32::from_bits(s.load(Ordering::Relaxed));
        }
        std::sync::atomic::fence(Ordering::Acquire);
        slot.block.load(Ordering::Relaxed) == block
    }

    /// The blocks to have loaded, most urgent first.
    fn wanted(&self) -> Vec<u64> {
        let last = self.frames.div_ceil(BLOCK as u64);
        let mut wanted = Vec::with_capacity(SLOTS);
        for c in self.cursors.iter().map(|c| c.load(Ordering::Relaxed)).filter(|c| *c != NONE) {
            let ahead = c..=c.saturating_add(AHEAD as u64);
            for b in ahead.chain(c.checked_sub(1)) {
                if b < last && !wanted.contains(&b) {
                    wanted.push(b);
                }
            }
        }
        wanted
    }

    /// Load one missing wanted block; `false` when there was nothing to load.
    fn load_one(&self, reader: &mut WavReader) -> bool {
        let wanted = self.wanted();
        let Some(&block) = wanted.iter().find(|b| !self.holds(**b)) else { return false };
        let victim = self
            .slots
            .iter()
            .find(|s| s.block.load(Ordering::Relaxed) == NONE)
            .or_else(|| self.slots.iter().find(|s| !wanted.contains(&s.block.load(Ordering::Relaxed))));
        let Some(slot) = victim else { return false };
        slot.block.store(NONE, Ordering::Release);
        let got = reader.read(block * BLOCK as u64, BLOCK);
        let channels = match got {
            Ok(c) => c,
            Err(e) => {
                if !self.failed.swap(true, Ordering::Relaxed) {
                    log::warn!("cannot read streamed audio: {e}");
                }
                Vec::new()
            }
        };
        let data = slot.data.get_or_init(|| (0..self.channels * BLOCK).map(|_| AtomicU32::new(0)).collect());
        for ch in 0..self.channels {
            let samples = channels.get(ch).map_or(&[][..], Vec::as_slice);
            for (i, cell) in data.iter().skip(ch * BLOCK).take(BLOCK).enumerate() {
                cell.store(samples.get(i).copied().unwrap_or(0.0).to_bits(), Ordering::Relaxed);
            }
        }
        slot.block.store(block, Ordering::Release);
        true
    }
}

fn prefetch(cache: &Cache, mut reader: WavReader) {
    while !cache.stop.load(Ordering::Relaxed) {
        if !cache.load_one(&mut reader) {
            std::thread::park_timeout(IDLE);
        }
    }
}

/// A WAV file read from disk as it plays. See the module documentation.
pub struct DiskStream {
    cache: Arc<Cache>,
    thread: std::thread::Thread,
}

impl std::fmt::Debug for DiskStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskStream").field("channels", &self.cache.channels).field("frames", &self.cache.frames).finish()
    }
}

impl DiskStream {
    /// Open `path` for streaming. `Ok(None)` when the file is not one that can be streamed (not a
    /// PCM or float WAV): decode it whole instead.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Option<DiskStream>, String> {
        let Some(reader) = WavReader::open(path).map_err(|e| e.to_string())? else { return Ok(None) };
        let info = reader.info().clone();
        let channels = usize::from(info.channels);
        let slot = || Slot { block: AtomicU64::new(NONE), data: OnceLock::new() };
        let cache = Arc::new(Cache {
            channels,
            frames: info.frames,
            sample_rate: info.sample_rate,
            slots: (0..SLOTS).map(|_| slot()).collect(),
            cursors: std::array::from_fn(|_| AtomicU64::new(NONE)),
            next_cursor: AtomicUsize::new(0),
            misses: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let worker = Arc::clone(&cache);
        let handle =
            std::thread::Builder::new().name("liviwave-stream".into()).spawn(move || prefetch(&worker, reader)).map_err(|e| e.to_string())?;
        Ok(Some(DiskStream { cache, thread: handle.thread().clone() }))
    }

    pub fn channels(&self) -> usize {
        self.cache.channels
    }

    pub fn frames(&self) -> u64 {
        self.cache.frames
    }

    pub fn sample_rate(&self) -> u32 {
        self.cache.sample_rate
    }

    /// Blocks that were read before they were loaded (each played as silence) so far.
    pub fn misses(&self) -> u64 {
        self.cache.misses.load(Ordering::Relaxed)
    }

    /// Whether reading the file has failed (the unreadable blocks play as silence).
    pub fn failed(&self) -> bool {
        self.cache.failed.load(Ordering::Relaxed)
    }

    /// Fill `out` with channel `ch` from frame `start` on; frames outside the audio, on a missing
    /// channel, or not loaded yet read as silence. Never blocks: for the audio thread.
    pub fn read(&self, ch: usize, start: i64, out: &mut [f32]) {
        out.fill(0.0);
        let c = &*self.cache;
        if ch >= c.channels {
            return;
        }
        let skip = usize::try_from(start.saturating_neg().max(0)).unwrap_or(usize::MAX);
        let from = u64::try_from(start.max(0)).unwrap_or(0);
        let Some(dst) = out.get_mut(skip..) else { return };
        let n = usize::try_from(c.frames.saturating_sub(from)).map_or(dst.len(), |left| left.min(dst.len()));
        let mut done = 0;
        while done < n {
            let pos = from.saturating_add(done as u64);
            let (block, offset) = (pos / BLOCK as u64, (pos % BLOCK as u64) as usize);
            let take = (BLOCK - offset).min(n - done);
            c.touch(block);
            let Some(part) = dst.get_mut(done..done + take) else { return };
            if !c.copy(block, ch, offset, part) {
                part.fill(0.0);
                c.misses.fetch_add(1, Ordering::Relaxed);
            }
            done += take;
        }
    }

    /// Load the audio around `frame` before playback starts there, waiting up to `timeout`.
    /// `true` when the first blocks are loaded.
    pub fn prime(&self, frame: u64, timeout: Duration) -> bool {
        let c = &*self.cache;
        let block = frame / BLOCK as u64;
        c.touch(block);
        self.thread.unpark();
        let need: Vec<u64> = (block..block + 2).filter(|b| b * (BLOCK as u64) < c.frames).collect();
        let until = Instant::now() + timeout;
        while !need.iter().all(|b| c.holds(*b)) {
            if Instant::now() >= until {
                return false;
            }
            self.thread.unpark();
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }
}

impl Drop for DiskStream {
    fn drop(&mut self) {
        self.cache.stop.store(true, Ordering::Relaxed);
        self.thread.unpark();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use liviwave_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat, encode};

    const SECOND: Duration = Duration::from_secs(1);

    /// A stereo WAV of `frames` frames whose samples tell their position, and what it holds.
    fn file(name: &str, frames: usize) -> (std::path::PathBuf, Vec<Vec<f32>>) {
        let mut b = AudioBuffer::new(48_000, 2, frames);
        for i in 0..frames {
            b.channels[0][i] = (i % 4096) as f32 / 4096.0;
            b.channels[1][i] = 1.0 - (i % 2048) as f32 / 2048.0;
        }
        let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None };
        let path = std::env::temp_dir().join(format!("liviwave-stream-{}-{name}.wav", std::process::id()));
        std::fs::write(&path, encode(&b, &opts).unwrap()).unwrap();
        (path, b.channels)
    }

    fn open(path: &std::path::Path) -> DiskStream {
        DiskStream::open(path).unwrap().unwrap()
    }

    #[test]
    fn sample_memory_is_allocated_only_as_blocks_are_loaded() {
        let (path, _) = file("lazy", BLOCK * 3);
        let s = open(&path);
        let held = |s: &DiskStream| s.cache.slots.iter().filter(|slot| slot.data.get().is_some()).count();
        assert_eq!(held(&s), 0, "an unplayed file holds no sample memory");
        assert!(s.prime(0, SECOND));
        let n = held(&s);
        assert!((1..SLOTS).contains(&n), "{n} of {SLOTS} slots allocated");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_primed_stream_reads_the_file_across_block_boundaries() {
        let (path, want) = file("prime", BLOCK * 3 + 100);
        let s = open(&path);
        assert_eq!((s.channels(), s.frames(), s.sample_rate()), (2, (BLOCK * 3 + 100) as u64, 48_000));
        assert!(s.prime(0, SECOND));
        let mut out = vec![0.0; 600];
        for (ch, from) in [(0, 0usize), (1, BLOCK - 300), (0, BLOCK * 2 - 50)] {
            // Wait for the blocks a read needs: only the first ones were primed.
            let deadline = Instant::now() + SECOND;
            loop {
                s.read(ch, from as i64, &mut out);
                if out == want[ch][from..from + 600] || Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            assert_eq!(out, want[ch][from..from + 600], "channel {ch} from {from}");
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn frames_outside_the_file_and_missing_channels_are_silent() {
        let (path, want) = file("edges", 1_000);
        let s = open(&path);
        assert!(s.prime(0, SECOND));
        let mut out = [9.0; 6];
        s.read(0, -2, &mut out);
        assert_eq!(out[..2], [0.0, 0.0]);
        assert_eq!(out[2..], want[0][..4]);
        s.read(0, 997, &mut out);
        assert_eq!(out[..3], want[0][997..]);
        assert_eq!(out[3..], [0.0; 3]);
        s.read(0, i64::MIN, &mut out);
        assert_eq!(out, [0.0; 6]);
        s.read(0, i64::MAX, &mut out);
        assert_eq!(out, [0.0; 6]);
        s.read(7, 0, &mut out);
        assert_eq!(out, [0.0; 6]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn playing_in_real_time_from_a_primed_position_loses_nothing() {
        let (path, want) = file("play", BLOCK * 12);
        let s = open(&path);
        assert!(s.prime(BLOCK as u64 * 2, SECOND));
        let mut pos = BLOCK * 2;
        let mut out = vec![0.0; 512];
        while pos + 512 <= BLOCK * 11 {
            s.read(1, pos as i64, &mut out);
            assert_eq!(out, want[1][pos..pos + 512], "at {pos}");
            pos += 512;
            // Far faster than real time (512 frames last 10 ms), to keep the test short.
            std::thread::sleep(Duration::from_micros(500));
        }
        assert_eq!(s.misses(), 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_block_that_is_not_loaded_yet_plays_as_silence_and_is_counted() {
        let (path, _) = file("miss", BLOCK * 2);
        let s = open(&path);
        let mut out = [1.0; 64];
        s.read(0, 10, &mut out);
        assert_eq!(out, [0.0; 64]);
        assert_eq!(s.misses(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn two_places_in_one_file_stay_loaded_together() {
        let (path, want) = file("two", BLOCK * 40);
        let s = open(&path);
        let (a, b) = (BLOCK * 3, BLOCK * 30);
        let mut out = vec![0.0; 256];
        let deadline = Instant::now() + 2 * SECOND;
        // Two clips read in turn, as the mix does; both get their audio.
        let (mut got_a, mut got_b) = (false, false);
        while Instant::now() < deadline && !(got_a && got_b) {
            s.read(0, a as i64, &mut out);
            got_a = out == want[0][a..a + 256];
            s.read(0, b as i64, &mut out);
            got_b = out == want[0][b..b + 256];
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(got_a && got_b, "a {got_a}, b {got_b}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_file_that_is_not_a_wav_is_refused() {
        let path = std::env::temp_dir().join(format!("liviwave-stream-{}-junk.wav", std::process::id()));
        std::fs::write(&path, b"nothing like a wav file").unwrap();
        assert!(DiskStream::open(&path).is_err());
        let _ = std::fs::remove_file(path);
    }
}
