//! Random-access reading of a WAV file on disk: blocks of frames are decoded on demand, so a
//! long file need not be decoded into memory whole.

use crate::pcm::{PcmKind, deinterleave};
use crate::wav::{WavParse, parse_at};
use crate::{AudioError, AudioInfo, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// How much of the start of a file is read to find its `fmt` and `data` chunks.
const HEAD: u64 = 1 << 20;

/// An open PCM or float WAV, RF64 and BWF included, read a range of frames at a time.
pub struct WavReader {
    file: File,
    info: AudioInfo,
    kind: PcmKind,
    data_start: u64,
    block: usize,
    bytes: Vec<u8>,
}

impl WavReader {
    /// Open `path`. `Ok(None)` for a file this reader cannot stream (another codec, or a data size
    /// that is only known from the end of the file); the caller decodes such a file whole.
    pub fn open(path: impl AsRef<Path>) -> Result<Option<WavReader>> {
        let mut file = File::open(path)?;
        let total = file.metadata()?.len();
        let mut head = vec![0u8; usize::try_from(total.min(HEAD)).unwrap_or(0)];
        file.read_exact(&mut head)?;
        let layout = match parse_at(&head, total) {
            Ok(WavParse::Native(l)) => l,
            Ok(WavParse::Foreign { .. }) | Err(AudioError::Unsupported(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let block = layout
            .kind
            .bytes()
            .checked_mul(usize::from(layout.info.channels))
            .filter(|b| *b > 0)
            .ok_or_else(|| AudioError::Malformed("zero-sized sample frame".into()))?;
        let data_start = layout.data_start as u64;
        Ok(Some(WavReader { file, info: layout.info, kind: layout.kind, data_start, block, bytes: Vec::new() }))
    }

    pub fn info(&self) -> &AudioInfo {
        &self.info
    }

    /// Decode up to `frames` frames from frame `start` into planar channels. Fewer come back at
    /// the end of the audio, none past it.
    pub fn read(&mut self, start: u64, frames: usize) -> Result<Vec<Vec<f32>>> {
        let left = self.info.frames.saturating_sub(start);
        let frames = usize::try_from(left).map_or(frames, |l| l.min(frames));
        let channels = usize::from(self.info.channels);
        if frames == 0 {
            return Ok(vec![Vec::new(); channels]);
        }
        let at = start
            .checked_mul(self.block as u64)
            .and_then(|o| o.checked_add(self.data_start))
            .ok_or_else(|| AudioError::TooLarge("file offset overflows".into()))?;
        self.file.seek(SeekFrom::Start(at))?;
        self.bytes.resize(frames.saturating_mul(self.block), 0);
        self.file.read_exact(&mut self.bytes)?;
        deinterleave(&self.bytes, self.kind, channels, frames as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioBuffer, BitDepth, EncodeOptions, FileFormat, decode, encode};

    fn ramp_file(name: &str, frames: usize, depth: BitDepth) -> (std::path::PathBuf, AudioBuffer) {
        let mut b = AudioBuffer::new(48_000, 2, frames);
        for i in 0..frames {
            b.channels[0][i] = (i % 1000) as f32 / 1000.0;
            b.channels[1][i] = -((i % 500) as f32) / 1000.0;
        }
        let opts = EncodeOptions { format: FileFormat::Wav, bit_depth: depth, dither: false, bwf: None };
        let bytes = encode(&b, &opts).unwrap();
        let path = std::env::temp_dir().join(format!("liviwave-wavreader-{}-{name}.wav", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();
        // What the whole-file decoder makes of it, to compare blocks against.
        let (_, whole) = decode(&bytes, Some("wav")).unwrap();
        (path, whole)
    }

    #[test]
    fn blocks_match_the_whole_file_decoded() {
        for (name, depth) in [("f32", BitDepth::Float32), ("i16", BitDepth::Int16), ("i24", BitDepth::Int24)] {
            let (path, whole) = ramp_file(name, 5_000, depth);
            let mut r = WavReader::open(&path).unwrap().unwrap();
            assert_eq!((r.info().frames, r.info().channels, r.info().sample_rate), (5_000, 2, 48_000));
            let got = r.read(1_234, 700).unwrap();
            assert_eq!(got[0], whole.channels[0][1_234..1_934], "{name}");
            assert_eq!(got[1], whole.channels[1][1_234..1_934], "{name}");
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn a_read_stops_at_the_end_and_past_it_is_empty() {
        let (path, whole) = ramp_file("end", 1_000, BitDepth::Int16);
        let mut r = WavReader::open(&path).unwrap().unwrap();
        let tail = r.read(900, 500).unwrap();
        assert_eq!(tail[1], whole.channels[1][900..]);
        assert!(r.read(1_000, 10).unwrap().iter().all(Vec::is_empty));
        assert!(r.read(u64::MAX, 10).unwrap().iter().all(Vec::is_empty));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn something_that_is_not_a_wav_is_an_error() {
        let path = std::env::temp_dir().join(format!("liviwave-wavreader-{}-junk.wav", std::process::id()));
        std::fs::write(&path, b"not a wav file at all, just text").unwrap();
        assert!(WavReader::open(&path).is_err());
        let _ = std::fs::remove_file(path);
    }
}
