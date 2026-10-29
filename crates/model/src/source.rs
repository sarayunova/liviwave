//! Audio sources (files in the session's Audio Files folder) and their decoded data, and the
//! movies video clips reference.

use crate::SourceId;
use crate::stream::DiskStream;
use liviwave_audio_io::{AudioBuffer, FileFormat, WavReader, peaks::Peaks, peaks::PeaksBuilder};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Metadata for an audio file used by the session (serialised).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Source {
    pub id: SourceId,
    pub name: String,
    /// Path relative to the session folder (or absolute for referenced media).
    pub path: String,
    pub channels: u16,
    pub frames: u64,
    pub sample_rate: u32,
    pub format: FileFormat,
    /// Original BWF time stamp (samples), if any.
    #[serde(default)]
    pub time_reference: u64,
    /// True when the file has not been written to disk yet (recorded/rendered this session).
    #[serde(default)]
    pub unsaved: bool,
}

/// A movie used by Video track clips (serialised). The pictures themselves are never stored in
/// the session: the UI opens the movie from `path` and decodes frames on demand.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct VideoSource {
    pub id: SourceId,
    pub name: String,
    /// Absolute path of the movie (referenced, never copied into the session folder).
    pub path: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// Frames per second (0 = unknown).
    #[serde(default)]
    pub frame_rate: f64,
    /// Picture length in seconds.
    #[serde(default)]
    pub duration: f64,
    /// Codec id (`h264`, `prores`, `mjpeg`, `hevc`, …).
    #[serde(default)]
    pub codec: String,
    /// Why the movie is not opened, when its stored path was refused on opening the session.
    #[serde(skip)]
    pub offline: Option<String>,
}

impl VideoSource {
    /// Display aspect ratio (width / height), 16:9 when unknown.
    pub fn aspect(&self) -> f64 {
        if self.width == 0 || self.height == 0 { 16.0 / 9.0 } else { f64::from(self.width) / f64::from(self.height) }
    }
}

/// Decoded audio plus waveform overviews (not serialised).
///
/// A source streamed from disk ([`SourceAudio::streamed`]) has no samples in `buffer`: only its
/// sample rate. Read it through [`SourceAudio::read`], and ask [`SourceAudio::whole`] for all of
/// it where the whole buffer is needed.
#[derive(Debug)]
pub struct SourceAudio {
    pub buffer: AudioBuffer,
    pub peaks: Vec<Peaks>,
    stream: Option<(DiskStream, std::path::PathBuf)>,
}

impl SourceAudio {
    pub fn new(buffer: AudioBuffer) -> Self {
        let peaks = buffer.channels.iter().map(|c| Peaks::build(c)).collect();
        SourceAudio { buffer, peaks, stream: None }
    }

    /// The audio of the WAV file at `path`, read from disk as it plays. The overviews are built
    /// from one pass over the file. `Ok(None)` when the file cannot be streamed.
    pub fn streamed(path: impl AsRef<std::path::Path>) -> Result<Option<SourceAudio>, String> {
        /// Frames read at a time while the overviews are built.
        const PASS: usize = 1 << 16;
        let path = path.as_ref();
        let Some(stream) = DiskStream::open(path)? else { return Ok(None) };
        let Some(mut reader) = WavReader::open(path).map_err(|e| e.to_string())? else { return Ok(None) };
        let mut builders: Vec<PeaksBuilder> = (0..stream.channels()).map(|_| PeaksBuilder::default()).collect();
        let mut at = 0u64;
        while at < stream.frames() {
            let block = reader.read(at, PASS).map_err(|e| e.to_string())?;
            let got = block.first().map_or(0, Vec::len);
            if got == 0 {
                break;
            }
            for (b, ch) in builders.iter_mut().zip(&block) {
                b.push(ch);
            }
            at += got as u64;
        }
        let peaks = builders.into_iter().map(PeaksBuilder::finish).collect();
        let buffer = AudioBuffer { sample_rate: stream.sample_rate(), channels: Vec::new() };
        Ok(Some(SourceAudio { buffer, peaks, stream: Some((stream, path.to_path_buf())) }))
    }

    /// Whether the samples are read from disk as they play, not held in `buffer`.
    pub fn is_streamed(&self) -> bool {
        self.stream.is_some()
    }

    pub fn frames(&self) -> usize {
        self.stream.as_ref().map_or_else(|| self.buffer.frames(), |(s, _)| usize::try_from(s.frames()).unwrap_or(usize::MAX))
    }

    pub fn channel_count(&self) -> usize {
        self.stream.as_ref().map_or_else(|| self.buffer.channels.len(), |(s, _)| s.channels())
    }

    pub fn sample_rate(&self) -> u32 {
        self.buffer.sample_rate
    }

    /// Load the audio around `frame` before playback starts there (a streamed source only).
    pub fn prime(&self, frame: u64) {
        if let Some((s, _)) = &self.stream {
            s.prime(frame, std::time::Duration::from_millis(250));
        }
    }

    /// Blocks of a streamed source that were not loaded in time and played as silence so far.
    pub fn misses(&self) -> u64 {
        self.stream.as_ref().map_or(0, |(s, _)| s.misses())
    }

    /// The WAV file a streamed source plays from.
    pub fn stream_path(&self) -> Option<&std::path::Path> {
        self.stream.as_ref().map(|(_, p)| p.as_path())
    }

    /// All the audio in memory: `buffer` itself, or for a streamed source the file decoded whole.
    pub fn whole(&self) -> Result<std::borrow::Cow<'_, AudioBuffer>, String> {
        let Some((_, path)) = &self.stream else { return Ok(std::borrow::Cow::Borrowed(&self.buffer)) };
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (_, buf) = liviwave_audio_io::decode(&bytes, Some("wav")).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(std::borrow::Cow::Owned(buf))
    }

    pub fn channel(&self, ch: usize) -> Option<&[f32]> {
        self.buffer.channels.get(ch).map(Vec::as_slice)
    }

    /// Fill `out` with channel `ch` from frame `start` on; frames outside the audio read as
    /// silence. The one way the mix reads sources, so how they are stored can change behind it.
    pub fn read(&self, ch: usize, start: i64, out: &mut [f32]) {
        if let Some((s, _)) = &self.stream {
            s.read(ch, start, out);
            return;
        }
        out.fill(0.0);
        let Some(src) = self.channel(ch) else { return };
        let from = start.max(0);
        let Ok(from_us) = usize::try_from(from) else { return };
        let skip = usize::try_from(from.saturating_sub(start)).unwrap_or(usize::MAX);
        let Some(dst) = out.get_mut(skip..) else { return };
        let Some(have) = src.get(from_us..) else { return };
        let n = dst.len().min(have.len());
        if let (Some(d), Some(h)) = (dst.get_mut(..n), have.get(..n)) {
            d.copy_from_slice(h);
        }
    }
}

/// Shared decoded audio, keyed by source id.
#[derive(Debug, Clone, Default)]
pub struct SourcePool {
    map: BTreeMap<SourceId, Arc<SourceAudio>>,
}

impl SourcePool {
    pub fn insert(&mut self, id: SourceId, audio: Arc<SourceAudio>) {
        self.map.insert(id, audio);
    }
    pub fn get(&self, id: SourceId) -> Option<&Arc<SourceAudio>> {
        self.map.get(&id)
    }
    pub fn remove(&mut self, id: SourceId) -> Option<Arc<SourceAudio>> {
        self.map.remove(&id)
    }
    pub fn contains(&self, id: SourceId) -> bool {
        self.map.contains_key(&id)
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn ids(&self) -> impl Iterator<Item = SourceId> + '_ {
        self.map.keys().copied()
    }
}

impl PartialEq for SourcePool {
    fn eq(&self, other: &Self) -> bool {
        self.map.len() == other.map.len() && self.map.iter().zip(other.map.iter()).all(|((a, x), (b, y))| a == b && Arc::ptr_eq(x, y))
    }
}

#[cfg(test)]
mod read_tests {
    use super::*;

    fn ramp() -> SourceAudio {
        let mut b = AudioBuffer::new(48_000, 2, 6);
        for (i, x) in b.channels[0].iter_mut().enumerate() {
            *x = i as f32 + 1.0;
        }
        SourceAudio::new(b)
    }

    #[test]
    fn read_copies_the_frames_asked_for() {
        let mut out = [9.0; 3];
        ramp().read(0, 2, &mut out);
        assert_eq!(out, [3.0, 4.0, 5.0]);
    }

    #[test]
    fn read_is_silent_before_the_start_after_the_end_and_on_a_missing_channel() {
        let a = ramp();
        let mut out = [9.0; 4];
        a.read(0, -2, &mut out);
        assert_eq!(out, [0.0, 0.0, 1.0, 2.0]);
        a.read(0, 4, &mut out);
        assert_eq!(out, [5.0, 6.0, 0.0, 0.0]);
        a.read(0, i64::MAX, &mut out);
        assert_eq!(out, [0.0; 4]);
        a.read(0, i64::MIN, &mut out);
        assert_eq!(out, [0.0; 4]);
        a.read(5, 0, &mut out);
        assert_eq!(out, [0.0; 4]);
    }
}

#[cfg(test)]
mod streamed_tests {
    use super::*;
    use liviwave_audio_io::{BitDepth, EncodeOptions, encode};

    #[test]
    fn a_streamed_source_reads_and_peaks_like_the_same_audio_in_memory() {
        let frames = 70_000;
        let mut b = AudioBuffer::new(48_000, 2, frames);
        for i in 0..frames {
            b.channels[0][i] = (i % 977) as f32 / 977.0 - 0.5;
            b.channels[1][i] = ((i * 7) % 311) as f32 / 311.0;
        }
        let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Float32, dither: false, bwf: None };
        let path = std::env::temp_dir().join(format!("liviwave-streamed-{}.wav", std::process::id()));
        std::fs::write(&path, encode(&b, &opts).unwrap()).unwrap();

        let ram = SourceAudio::new(b);
        let disk = SourceAudio::streamed(&path).unwrap().unwrap();
        assert!(disk.is_streamed() && !ram.is_streamed());
        assert_eq!((disk.frames(), disk.channel_count(), disk.sample_rate()), (frames, 2, 48_000));
        assert!(disk.buffer.channels.is_empty());
        assert_eq!(disk.peaks, ram.peaks);

        disk.prime(40_000);
        let (mut want, mut got) = ([0.0; 300], [0.0; 300]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            disk.read(1, 40_000, &mut got);
            ram.read(1, 40_000, &mut want);
            if got == want {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(got, want);
        assert_eq!(disk.whole().unwrap().channels, ram.buffer.channels);
        assert!(matches!(ram.whole().unwrap(), std::borrow::Cow::Borrowed(_)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_file_that_cannot_be_streamed_is_not() {
        let path = std::env::temp_dir().join(format!("liviwave-streamed-{}-junk.wav", std::process::id()));
        std::fs::write(&path, b"not audio").unwrap();
        assert!(SourceAudio::streamed(&path).is_err());
        let _ = std::fs::remove_file(path);
    }
}
