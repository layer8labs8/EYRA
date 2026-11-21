use anyhow::Result;
use std::fs;
use std::path::Path;

/// Encodes mono or multi-channel f32 samples ([-1.0, 1.0]) into a standard 16-bit PCM WAV byte buffer.
pub fn encode_wav_pcm16(samples: &[f32], sample_rate: u32, channels: u16) -> Vec<u8> {
    let ch = channels.max(1);
    let bits_per_sample: u16 = 16;
    let byte_rate = sample_rate * (ch as u32) * (bits_per_sample as u32 / 8);
    let block_align = ch * (bits_per_sample / 8);
    let data_bytes = (samples.len() * 2) as u32;
    let riff_size = 36u32.saturating_add(data_bytes);

    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");

    // fmt subchunk
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // AudioFormat = 1 (PCM)
    out.extend_from_slice(&ch.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits_per_sample.to_le_bytes());

    // data subchunk
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());

    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let v = (clamped * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }

    out
}

pub fn write_wav_pcm16(path: &Path, samples: &[f32], sample_rate: u32) -> Result<usize> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = encode_wav_pcm16(samples, sample_rate, 1);
    let len = bytes.len();
    fs::write(path, bytes)?;
    Ok(len)
}

/// Approximate multimodal LLM audio token cost (~32 tokens per second of audio).
pub fn estimate_audio_tokens(duration_sec: f32) -> usize {
    if duration_sec <= 0.0 {
        0
    } else {
        (duration_sec * 32.0).ceil() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_and_payload_are_valid() {
        let samples = vec![0.0f32, 0.5, -0.5, 1.2, -1.5];
        let wav = encode_wav_pcm16(&samples, 16_000, 1);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(wav.len(), 44 + samples.len() * 2);

        // Verify sample rate = 16000
        let sr = u32::from_le_bytes(wav[24..28].try_into().unwrap());
        assert_eq!(sr, 16_000);

        // Verify clamping on +1.2 and -1.5
        let s3 = i16::from_le_bytes(wav[50..52].try_into().unwrap());
        let s4 = i16::from_le_bytes(wav[52..54].try_into().unwrap());
        assert_eq!(s3, 32767);
        assert_eq!(s4, -32767);
    }

    #[test]
    fn audio_token_estimation_is_reasonable() {
        assert_eq!(estimate_audio_tokens(0.0), 0);
        assert_eq!(estimate_audio_tokens(10.0), 320);
    }
}
