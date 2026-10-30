//! Loopback measurement: find how late a known test signal comes back when an output is cabled
//! to an input. The signal is a linear sweep, whose autocorrelation is one sharp peak, so the
//! delay can be read to the sample even through noise, hum and a gain change.

use std::f32::consts::PI;

/// The test signal at `rate`: a 0.25 s linear sweep from 500 Hz to 8 kHz at -6 dB, faded in and
/// out so it starts and stops without a click.
pub fn test_signal(rate: u32) -> Vec<f32> {
    let n = (rate as usize / 4).max(16);
    let (f0, f1) = (500.0f32, 8000.0f32);
    let secs = n as f32 / rate.max(1) as f32;
    (0..n)
        .map(|i| {
            let t = i as f32 / rate.max(1) as f32;
            let phase = 2.0 * PI * (f0 * t + (f1 - f0) * t * t / (2.0 * secs));
            let fade = (i.min(n - 1 - i) as f32 / 64.0).min(1.0);
            0.5 * fade * phase.sin()
        })
        .collect()
}

/// How many samples after the start of `recorded` the `signal` begins, with how clearly it was
/// found (peak over the average, 1.0 = lost in the noise). `None` when the recording is too short
/// or silent.
pub fn find_delay(signal: &[f32], recorded: &[f32]) -> Option<(usize, f32)> {
    let span = recorded.len().checked_sub(signal.len())?;
    let energy: f32 = signal.iter().map(|x| x * x).sum();
    if energy <= 0.0 || recorded.iter().all(|x| x.abs() < 1e-6) {
        return None;
    }
    // Normalised by the recording's own energy under the window, so a loud tail can't win.
    let mut best = (0usize, 0.0f32);
    let mut sum = 0.0f64;
    let mut window_energy: f64 = recorded.get(..signal.len())?.iter().map(|x| f64::from(*x) * f64::from(*x)).sum();
    for lag in 0..=span {
        let window = recorded.get(lag..lag + signal.len())?;
        let dot: f32 = signal.iter().zip(window).map(|(a, b)| a * b).sum();
        let norm = (window_energy.max(0.0).sqrt() * f64::from(energy).sqrt()) as f32;
        let score = if norm > 1e-9 { dot / norm } else { 0.0 };
        sum += f64::from(score.abs());
        if score > best.1 {
            best = (lag, score);
        }
        if let (Some(out), Some(inn)) = (recorded.get(lag), recorded.get(lag + signal.len())) {
            window_energy += f64::from(*inn) * f64::from(*inn) - f64::from(*out) * f64::from(*out);
        }
    }
    let mean = (sum / (span + 1) as f64) as f32;
    (best.1 > 0.0).then(|| (best.0, best.1 / mean.max(1e-6)))
}

/// What a loopback measurement found.
#[derive(Debug, Clone, PartialEq)]
pub struct Measured {
    /// The round trip, in samples at the session rate.
    pub samples: u32,
    pub ms: f32,
    /// How clearly the signal was heard (see [`find_delay`]).
    pub clarity: f32,
    /// The input channel that heard it, from 0.
    pub channel: usize,
}

/// Clock and results shared between the two streams' callbacks.
#[cfg(not(target_os = "freebsd"))]
struct Probe {
    base: std::time::Instant,
    go: std::sync::atomic::AtomicBool,
    /// When the signal's first sample reaches the output, in ns since `base`, plus one; 0 = not yet.
    sent: std::sync::atomic::AtomicU64,
    /// When the first captured input sample was taken, likewise.
    heard_from: std::sync::atomic::AtomicU64,
    heard: std::sync::Mutex<Vec<f32>>,
}

#[cfg(not(target_os = "freebsd"))]
impl Probe {
    fn now(&self) -> u64 {
        nanos(self.base.elapsed())
    }
}

#[cfg(not(target_os = "freebsd"))]
fn nanos(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(not(target_os = "freebsd"))]
fn build_probe_output<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    probe: &std::sync::Arc<Probe>,
    signal: Vec<f32>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    use std::sync::atomic::Ordering;
    let (probe, channels, rate) = (std::sync::Arc::clone(probe), usize::from(config.channels).max(1), u64::from(config.sample_rate).max(1));
    let mut pos = 0usize;
    device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let t = info.timestamp();
            let lead = nanos(t.playback.saturating_duration_since(t.callback));
            let now = probe.now();
            let go = probe.go.load(Ordering::Acquire);
            for (i, frame) in data.chunks_mut(channels).enumerate() {
                let v = if go { signal.get(pos).copied().unwrap_or(0.0) } else { 0.0 };
                if go {
                    if pos == 0 {
                        let at = now.saturating_add(lead).saturating_add(i as u64 * 1_000_000_000 / rate);
                        let _ = probe.sent.compare_exchange(0, at.saturating_add(1), Ordering::Relaxed, Ordering::Relaxed);
                    }
                    pos += 1;
                }
                for o in frame {
                    *o = T::from_sample(v);
                }
            }
        },
        |e: cpal::Error| log::warn!("loopback output error: {e}"),
        None,
    )
}

#[cfg(not(target_os = "freebsd"))]
fn build_probe_input<T>(device: &cpal::Device, config: cpal::StreamConfig, probe: &std::sync::Arc<Probe>) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::Sample;
    use cpal::traits::DeviceTrait;
    use std::sync::atomic::Ordering;
    let probe = std::sync::Arc::clone(probe);
    device.build_input_stream(
        config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            let t = info.timestamp();
            let age = nanos(t.callback.saturating_duration_since(t.capture));
            let first = probe.now().saturating_sub(age);
            let _ = probe.heard_from.compare_exchange(0, first.saturating_add(1), Ordering::Relaxed, Ordering::Relaxed);
            let mut heard = probe.heard.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // Capacity was reserved up front: when it is full, the recording is over.
            if heard.len() + data.len() <= heard.capacity() {
                heard.extend(data.iter().map(|x| f32::from_sample(*x)));
            }
        },
        |e: cpal::Error| log::warn!("loopback input error: {e}"),
        None,
    )
}

/// Play the test signal out of the output `audio` chooses and listen for it on the input, with a
/// cable from one to the other. The round trip is the time from the signal reaching the output
/// to the same signal captured, read off the host clock and the latencies the two streams
/// report, so it is good to about one buffer period (cpal has no duplex stream to do better).
/// Blocks for about two seconds.
#[cfg(not(target_os = "freebsd"))]
pub fn measure(audio: &crate::AudioConfig, session_rate: u32) -> Result<Measured, String> {
    use cpal::traits::{DeviceTrait, StreamTrait};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    let (out_dev, out_cfg, out_fmt, _) = crate::stream::open_device(session_rate, 2, audio)?;
    let rate = out_cfg.sample_rate;
    let in_dev = crate::devices::device(&crate::devices::host(audio), audio.input.as_deref(), false).ok_or("no audio input device")?;
    let in_configs: Vec<cpal::SupportedStreamConfigRange> = in_dev.supported_input_configs().map(|c| c.collect()).unwrap_or_default();
    let in_sup = crate::record::pick_input_config(&in_configs, rate)
        .ok_or_else(|| format!("the input device cannot run at the output's {rate} Hz; choose a rate both offer in Setup › Playback Engine"))?;
    let in_fmt = in_sup.sample_format();
    let mut in_cfg = in_sup.config();
    in_cfg.buffer_size = crate::devices::buffer_size(audio.buffer_size, in_sup.buffer_size());
    let in_channels = usize::from(in_cfg.channels).max(1);

    let signal = test_signal(rate);
    let seconds = signal.len() / rate.max(1) as usize + 3;
    let probe = Arc::new(Probe {
        base: std::time::Instant::now(),
        go: AtomicBool::new(false),
        sent: AtomicU64::new(0),
        heard_from: AtomicU64::new(0),
        heard: Mutex::new(Vec::with_capacity(rate as usize * in_channels * seconds)),
    });
    let output = match out_fmt {
        cpal::SampleFormat::I32 => build_probe_output::<i32>(&out_dev, out_cfg, &probe, signal.clone()),
        cpal::SampleFormat::I16 => build_probe_output::<i16>(&out_dev, out_cfg, &probe, signal.clone()),
        _ => build_probe_output::<f32>(&out_dev, out_cfg, &probe, signal.clone()),
    }
    .map_err(|e| e.to_string())?;
    let input = match in_fmt {
        cpal::SampleFormat::I32 => build_probe_input::<i32>(&in_dev, in_cfg, &probe),
        cpal::SampleFormat::I16 => build_probe_input::<i16>(&in_dev, in_cfg, &probe),
        _ => build_probe_input::<f32>(&in_dev, in_cfg, &probe),
    }
    .map_err(|e| e.to_string())?;
    input.play().map_err(|e| e.to_string())?;
    output.play().map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_millis(400));
    probe.go.store(true, Ordering::Release);
    std::thread::sleep(Duration::from_millis(1200) + Duration::from_secs_f64(signal.len() as f64 / f64::from(rate.max(1))));
    drop((output, input));

    let (sent, from) = (probe.sent.load(Ordering::Relaxed), probe.heard_from.load(Ordering::Relaxed));
    if sent == 0 || from == 0 {
        return Err("the audio devices did not start; nothing was measured".into());
    }
    let heard = probe.heard.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let best = (0..in_channels)
        .filter_map(|ch| {
            let mono: Vec<f32> = heard.iter().skip(ch).step_by(in_channels).copied().collect();
            find_delay(&signal, &mono).map(|(lag, clarity)| (ch, lag, clarity))
        })
        .max_by(|a, b| a.2.total_cmp(&b.2));
    let Some((channel, lag, clarity)) = best.filter(|b| b.2 >= 4.0) else {
        return Err("the test signal did not come back; connect an output to an input and raise the input level".into());
    };
    let heard_at = (from - 1) as f64 + lag as f64 * 1e9 / f64::from(rate);
    let trip_ns = (heard_at - (sent - 1) as f64).max(0.0);
    let samples = (trip_ns * f64::from(session_rate) / 1e9).round() as u32;
    Ok(Measured { samples, ms: (trip_ns / 1e6) as f32, clarity, channel })
}

#[cfg(target_os = "freebsd")]
pub fn measure(_audio: &crate::AudioConfig, _session_rate: u32) -> Result<Measured, String> {
    Err("loopback measurement is not supported on this platform yet".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording(signal: &[f32], delay: usize, gain: f32, noise: f32) -> Vec<f32> {
        let mut seed = 12345u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let mut out: Vec<f32> = (0..delay + signal.len() + 4000).map(|_| noise * rnd()).collect();
        for (i, s) in signal.iter().enumerate() {
            if let Some(o) = out.get_mut(delay + i) {
                *o += gain * s;
            }
        }
        out
    }

    #[test]
    fn the_delay_is_found_to_the_sample() {
        let s = test_signal(48_000);
        for delay in [0, 1, 137, 2048, 5000] {
            let (found, _) = find_delay(&s, &recording(&s, delay, 1.0, 0.0)).expect("found");
            assert_eq!(found, delay);
        }
    }

    #[test]
    fn it_survives_noise_and_a_gain_change() {
        let s = test_signal(48_000);
        let (found, clarity) = find_delay(&s, &recording(&s, 777, 0.1, 0.2)).expect("found");
        assert_eq!(found, 777);
        assert!(clarity > 5.0, "clarity {clarity}");
    }

    #[test]
    fn silence_and_short_recordings_find_nothing() {
        let s = test_signal(48_000);
        assert!(find_delay(&s, &vec![0.0; 40_000]).is_none());
        assert!(find_delay(&s, &s[..100]).is_none());
    }
}
