pub mod vad;
pub mod wasapi;
pub mod wav;

use anyhow::Result;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use self::vad::{analyze_audio, trim_silence, DEFAULT_VAD_THRESHOLD_DB, TARGET_SAMPLE_RATE};
use self::wasapi::{capture_audio, AudioSource};
use self::wav::{estimate_audio_tokens, write_wav_pcm16};

const MAX_ONESHOT_CLIPS: usize = 24;

#[derive(Debug, Clone)]
pub struct ListenOptions {
    pub seconds: f32,
    pub source: AudioSource,
    pub trim_silence: bool,
    pub threshold_db: f32,
    pub out_path: Option<PathBuf>,
    /// Whether the user allowed microphone capture (see `SafetyConfig::allow_mic`).
    pub allow_mic: bool,
}

impl Default for ListenOptions {
    fn default() -> Self {
        Self {
            seconds: 8.0,
            source: AudioSource::Both,
            trim_silence: true,
            threshold_db: DEFAULT_VAD_THRESHOLD_DB,
            out_path: None,
            allow_mic: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SavedClip {
    pub channel: &'static str,
    pub device: String,
    pub raw_sec: f32,
    pub saved_sec: f32,
    pub rms_db: f32,
    pub label: &'static str,
    pub bytes: usize,
    pub est_tokens: usize,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ListenResult {
    pub summary: String,
    pub clips: Vec<SavedClip>,
    pub total_audio_tokens: usize,
    pub elapsed_ms: u64,
}

pub fn default_audio_dir() -> PathBuf {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        if !local_app_data.trim().is_empty() {
            return PathBuf::from(local_app_data).join("eyra").join("audio");
        }
    }
    std::env::temp_dir().join("eyra").join("audio")
}

/// Applies the microphone opt-in: `mic` alone is refused, `both` falls back to system audio only
/// with a note saying so.
pub fn effective_source(
    requested: AudioSource,
    allow_mic: bool,
) -> Result<(AudioSource, Option<String>)> {
    match (requested, allow_mic) {
        (AudioSource::Mic, false) => anyhow::bail!(
            "Microphone capture is off. Restart eyra with --allow-mic (or set EYRA_ALLOW_MIC=1) to let it record the microphone."
        ),
        (AudioSource::Both, false) => Ok((
            AudioSource::System,
            Some("microphone skipped: microphone capture is off (start eyra with --allow-mic)".to_string()),
        )),
        (s, _) => Ok((s, None)),
    }
}

/// Human-readable label for the recording badge.
pub fn source_label(source: AudioSource) -> &'static str {
    match source {
        AudioSource::System => "system audio",
        AudioSource::Mic => "microphone",
        AudioSource::Both => "system audio + microphone",
    }
}

pub fn listen(opts: &ListenOptions) -> Result<ListenResult> {
    let t0 = Instant::now();
    let duration = opts.seconds.clamp(1.0, 60.0);
    let (source, mic_note) = effective_source(opts.source, opts.allow_mic)?;
    // Visible while capturing; removed when this function returns
    let _badge = crate::indicator::RecordingIndicator::show(source_label(source));
    let mut captured = capture_audio(source, duration)?;
    captured.warnings.extend(mic_note);

    let audio_dir = default_audio_dir();
    let _ = fs::create_dir_all(&audio_dir);
    prune_old_wavs(&audio_dir, MAX_ONESHOT_CLIPS);

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    let mut clips = Vec::new();
    let mut total_audio_tokens = 0usize;

    if let Some(sys) = captured.system {
        let clip = process_and_save_channel(
            "system",
            &sys.device_name,
            &sys.samples_16k,
            duration,
            opts,
            &audio_dir,
            stamp,
        )?;
        total_audio_tokens += clip.est_tokens;
        clips.push(clip);
    }

    if let Some(mic) = captured.mic {
        let clip = process_and_save_channel(
            "mic",
            &mic.device_name,
            &mic.samples_16k,
            duration,
            opts,
            &audio_dir,
            stamp,
        )?;
        total_audio_tokens += clip.est_tokens;
        clips.push(clip);
    }

    let elapsed_ms = t0.elapsed().as_millis() as u64;
    let mut summary = format!(
        "LISTEN dur={:.1}s trim={} total_audio_tok=~{} ms={}\n",
        duration, opts.trim_silence, total_audio_tokens, elapsed_ms
    );

    for clip in &clips {
        if let Some(p) = &clip.path {
            let _ = writeln!(
                summary,
                "AUDIO[{}] {} {:.1}s/{:.1}s {:.1}dB bytes={} (~{}tok) dev=\"{}\" path={}",
                clip.channel,
                clip.label,
                clip.saved_sec,
                clip.raw_sec,
                clip.rms_db,
                clip.bytes,
                clip.est_tokens,
                crate::types::truncate_clean(&clip.device, 40),
                p.display()
            );
        } else {
            let _ = writeln!(
                summary,
                "AUDIO[{}] silence (0.0s/{:.1}s {:.1}dB, skipped) dev=\"{}\"",
                clip.channel,
                clip.raw_sec,
                clip.rms_db,
                crate::types::truncate_clean(&clip.device, 40)
            );
        }
    }

    for warn in &captured.warnings {
        let _ = writeln!(summary, "WARN: {}", warn);
    }

    Ok(ListenResult {
        summary,
        clips,
        total_audio_tokens,
        elapsed_ms,
    })
}

fn process_and_save_channel(
    channel: &'static str,
    device: &str,
    samples_16k: &[f32],
    requested_sec: f32,
    opts: &ListenOptions,
    audio_dir: &Path,
    stamp: u128,
) -> Result<SavedClip> {
    let raw_sec = if samples_16k.is_empty() {
        requested_sec
    } else {
        samples_16k.len() as f32 / TARGET_SAMPLE_RATE as f32
    };

    let (final_samples, metrics) = if opts.trim_silence {
        trim_silence(samples_16k, TARGET_SAMPLE_RATE, opts.threshold_db, 250)
    } else {
        let m = analyze_audio(samples_16k, TARGET_SAMPLE_RATE, opts.threshold_db);
        (samples_16k.to_vec(), m)
    };

    if final_samples.is_empty() || (opts.trim_silence && metrics.is_silent()) {
        return Ok(SavedClip {
            channel,
            device: device.to_string(),
            raw_sec,
            saved_sec: 0.0,
            rms_db: metrics.rms_db,
            label: "silence",
            bytes: 0,
            est_tokens: 0,
            path: None,
        });
    }

    let path = if let Some(custom) = &opts.out_path {
        if opts.source == AudioSource::Both {
            let stem = custom
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("listen");
            let ext = custom.extension().and_then(|s| s.to_str()).unwrap_or("wav");
            let mut p = custom.clone();
            p.set_file_name(format!("{}_{}.{}", stem, channel, ext));
            p
        } else {
            custom.clone()
        }
    } else {
        audio_dir.join(format!("listen_{}_{}.wav", stamp, channel))
    };

    let bytes = write_wav_pcm16(&path, &final_samples, TARGET_SAMPLE_RATE)?;
    let saved_sec = final_samples.len() as f32 / TARGET_SAMPLE_RATE as f32;
    let est_tokens = estimate_audio_tokens(saved_sec);

    Ok(SavedClip {
        channel,
        device: device.to_string(),
        raw_sec,
        saved_sec,
        rms_db: metrics.rms_db,
        label: metrics.label,
        bytes,
        est_tokens,
        path: Some(path),
    })
}

fn prune_old_wavs(dir: &Path, keep_max: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(PathBuf, SystemTime)> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("wav") {
                let mtime = e
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(UNIX_EPOCH);
                Some((p, mtime))
            } else {
                None
            }
        })
        .collect();

    if files.len() <= keep_max {
        return;
    }
    files.sort_by_key(|(_, t)| *t);
    let remove_count = files.len().saturating_sub(keep_max);
    for (p, _) in files.into_iter().take(remove_count) {
        let _ = fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microphone_needs_opt_in() {
        assert!(effective_source(AudioSource::Mic, false).is_err());
        let (src, note) = effective_source(AudioSource::Both, false).unwrap();
        assert_eq!(src, AudioSource::System);
        assert!(note.unwrap().contains("--allow-mic"));
        assert_eq!(
            effective_source(AudioSource::Both, true).unwrap().0,
            AudioSource::Both
        );
        assert_eq!(
            effective_source(AudioSource::System, false).unwrap(),
            (AudioSource::System, None)
        );
    }
}
