pub const TARGET_SAMPLE_RATE: u32 = 16_000;
pub const DEFAULT_VAD_THRESHOLD_DB: f32 = -42.0;

#[derive(Debug, Clone)]
pub struct AudioMetrics {
    pub active_sec: f32,
    pub rms_db: f32,
    pub label: &'static str,
}

impl AudioMetrics {
    pub fn is_silent(&self) -> bool {
        self.active_sec < 0.08 || self.label == "silence"
    }
}

/// Resamples mono f32 audio from `src_rate` to `dst_rate`.
/// Uses box-filter averaging when downsampling to avoid aliasing artifacts.
pub fn resample_mono(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if input.is_empty() || src_rate == 0 || dst_rate == 0 {
        return Vec::new();
    }
    if src_rate == dst_rate {
        return input.to_vec();
    }

    let ratio = src_rate as f64 / dst_rate as f64;
    let out_len = ((input.len() as f64) / ratio).round().max(1.0) as usize;
    let mut out = Vec::with_capacity(out_len);

    if ratio >= 1.5 {
        // Downsampling: average over the source window corresponding to each output sample
        for i in 0..out_len {
            let start = ((i as f64) * ratio).floor() as usize;
            let end = (((i + 1) as f64) * ratio).ceil() as usize;
            let end = end.min(input.len()).max(start + 1).min(input.len());
            if start >= input.len() {
                break;
            }
            let mut sum = 0.0f64;
            for &s in &input[start..end] {
                sum += s as f64;
            }
            out.push((sum / (end - start) as f64) as f32);
        }
    } else {
        // Linear interpolation
        for i in 0..out_len {
            let pos = (i as f64) * ratio;
            let idx = (pos.floor() as usize).min(input.len() - 1);
            let next = (idx + 1).min(input.len() - 1);
            let frac = (pos - idx as f64) as f32;
            out.push(input[idx] * (1.0 - frac) + input[next] * frac);
        }
    }

    out
}

pub fn amplitude_to_db(amp: f32) -> f32 {
    if amp <= 1e-6 {
        -100.0
    } else {
        20.0 * amp.log10()
    }
}

pub fn frame_rms_db(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return -100.0;
    }
    let sum_sq: f64 = frame.iter().map(|&x| (x as f64) * (x as f64)).sum();
    let rms = (sum_sq / frame.len() as f64).sqrt() as f32;
    amplitude_to_db(rms)
}

/// Analyzes audio signal energy, active duration, zero-crossing rate, and coarse acoustic class.
pub fn analyze_audio(samples: &[f32], sample_rate: u32, threshold_db: f32) -> AudioMetrics {
    if samples.is_empty() || sample_rate == 0 {
        return AudioMetrics {
            active_sec: 0.0,
            rms_db: -100.0,
            label: "silence",
        };
    }

    let duration_sec = samples.len() as f32 / sample_rate as f32;
    let frame_len = ((sample_rate as usize) / 50).max(1); // 20 ms frames

    let mut sum_sq = 0.0f64;
    let mut peak = 0.0f32;
    let mut crossings = 0usize;
    let mut prev_pos = samples[0] >= 0.0;

    for &s in samples {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        sum_sq += (s as f64) * (s as f64);
        let cur_pos = s >= 0.0;
        if cur_pos != prev_pos {
            crossings += 1;
            prev_pos = cur_pos;
        }
    }

    let rms = (sum_sq / samples.len() as f64).sqrt() as f32;
    let rms_db = amplitude_to_db(rms);
    let peak_db = amplitude_to_db(peak);
    let zcr = crossings as f32 / samples.len() as f32;

    let mut active_frames = 0usize;
    let mut total_frames = 0usize;
    for chunk in samples.chunks(frame_len) {
        total_frames += 1;
        if frame_rms_db(chunk) > threshold_db {
            active_frames += 1;
        }
    }

    let active_sec = if total_frames == 0 {
        0.0
    } else {
        duration_sec * (active_frames as f32 / total_frames as f32)
    };

    let label = if active_sec < 0.08 || peak_db < threshold_db {
        "silence"
    } else if peak_db > -6.0 && (peak_db - rms_db) > 16.0 {
        "audio_peak"
    } else if (0.02..=0.28).contains(&zcr) {
        "speech"
    } else {
        "music_or_ambient"
    };

    AudioMetrics {
        active_sec,
        rms_db,
        label,
    }
}

/// Strips long silent stretches using 20ms-frame energy VAD while preserving `pad_ms`
/// before and after active speech segments so words are never clipped.
pub fn trim_silence(
    samples: &[f32],
    sample_rate: u32,
    threshold_db: f32,
    pad_ms: u32,
) -> (Vec<f32>, AudioMetrics) {
    let raw_metrics = analyze_audio(samples, sample_rate, threshold_db);
    if samples.is_empty() || sample_rate == 0 || raw_metrics.is_silent() {
        return (Vec::new(), raw_metrics);
    }

    let frame_len = ((sample_rate as usize) / 50).max(1); // 20 ms
    let pad_frames = ((pad_ms as usize) / 20).max(1);

    let frames: Vec<&[f32]> = samples.chunks(frame_len).collect();
    let mut keep = vec![false; frames.len()];

    for (i, frame) in frames.iter().enumerate() {
        if frame_rms_db(frame) > threshold_db {
            let start = i.saturating_sub(pad_frames);
            let end = (i + pad_frames + 1).min(frames.len());
            keep[start..end].fill(true);
        }
    }

    // Bridge short silent gaps (< 300ms = 15 frames) for natural cadence
    let mut idx = 0;
    while idx < keep.len() {
        if !keep[idx] {
            let gap_start = idx;
            while idx < keep.len() && !keep[idx] {
                idx += 1;
            }
            let gap_len = idx - gap_start;
            if gap_start > 0 && idx < keep.len() && gap_len <= 15 {
                keep[gap_start..idx].fill(true);
            }
        } else {
            idx += 1;
        }
    }

    let mut out = Vec::with_capacity(samples.len());
    for (i, frame) in frames.iter().enumerate() {
        if keep[i] {
            out.extend_from_slice(frame);
        }
    }

    let trimmed_metrics = analyze_audio(&out, sample_rate, threshold_db);
    (out, trimmed_metrics)
}

/// Mixes two equal-rate mono buffers into one soft-clamped mono buffer.
#[allow(dead_code)]
pub fn mix_mono(a: &[f32], b: &[f32]) -> Vec<f32> {
    let len = a.len().max(b.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let sa = a.get(i).copied().unwrap_or(0.0);
        let sb = b.get(i).copied().unwrap_or(0.0);
        out.push((sa + sb).clamp(-1.0, 1.0));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    fn sine_wave(freq_hz: f32, sample_rate: u32, duration_sec: f32, amp: f32) -> Vec<f32> {
        let n = (sample_rate as f32 * duration_sec) as usize;
        (0..n)
            .map(|i| amp * (2.0 * PI * freq_hz * (i as f32) / (sample_rate as f32)).sin())
            .collect()
    }

    #[test]
    fn resample_48k_to_16k_preserves_duration() {
        let s48 = sine_wave(440.0, 48_000, 1.0, 0.5);
        let s16 = resample_mono(&s48, 48_000, 16_000);
        assert_eq!(s16.len(), 16_000);
        let m = analyze_audio(&s16, 16_000, DEFAULT_VAD_THRESHOLD_DB);
        assert!(!m.is_silent());
        assert!(m.rms_db > -15.0);
    }

    #[test]
    fn vad_trims_long_silence_and_keeps_speech() {
        let mut buf = vec![0.0f32; 16_000 * 2]; // 2s silence
        buf.extend(sine_wave(500.0, 16_000, 1.0, 0.35)); // 1s active
        buf.extend(vec![0.0f32; 16_000 * 2]); // 2s silence

        let (trimmed, metrics) = trim_silence(&buf, 16_000, DEFAULT_VAD_THRESHOLD_DB, 200);
        assert!(!metrics.is_silent());
        // Should trim ~5s down to ~1.4s (1s active + 200ms padding on each side)
        assert!(trimmed.len() < 16_000 * 2);
        assert!(trimmed.len() > 16_000);
    }

    #[test]
    fn pure_silence_returns_empty_trimmed_buffer() {
        let silent = vec![0.0f32; 16_000];
        let (trimmed, metrics) = trim_silence(&silent, 16_000, DEFAULT_VAD_THRESHOLD_DB, 200);
        assert!(trimmed.is_empty());
        assert!(metrics.is_silent());
        assert_eq!(metrics.label, "silence");
    }
}
