use crate::ear::vad::{resample_mono, TARGET_SAMPLE_RATE};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use windows::core::GUID;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EDataFlow, IAudioCaptureClient, IAudioClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioSource {
    System,
    Mic,
    Both,
}

impl AudioSource {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "system" | "sys" | "loopback" | "speaker" | "speakers" => Self::System,
            "mic" | "microphone" | "input" => Self::Mic,
            _ => Self::Both,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Loopback,
    Mic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    pub name: String,
    pub kind: String,
    pub is_default: bool,
}

impl AudioDeviceInfo {
    pub fn to_compact_line(&self) -> String {
        format!(
            "[{}] {}{}",
            self.kind,
            self.name,
            if self.is_default { " [DEFAULT]" } else { "" }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleEncoding {
    Float32,
    Pcm16,
    Pcm32,
}

pub struct WasapiStream {
    audio_client: IAudioClient,
    capture_client: IAudioCaptureClient,
    pub sample_rate: u32,
    channels: u16,
    encoding: SampleEncoding,
    pub device_name: String,
    /// QPC time (100 ns units) when the stream was started.
    start_100ns: u64,
    /// Mono frames handed out so far (audio + inserted silence), at `sample_rate`.
    emitted: Cell<u64>,
}

/// Current QueryPerformanceCounter time in 100 ns units, the clock WASAPI stamps packets with.
fn qpc_now_100ns() -> u64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut count, mut freq) = (0i64, 0i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    if freq <= 0 {
        return 0;
    }
    ((count as u128) * 10_000_000 / (freq as u128)) as u64
}

/// Frames of silence needed to bring `emitted` up to `target`: zero within `tolerance`
/// (timestamp jitter), capped at `cap` (guards against bogus timestamps).
fn silence_gap(emitted: u64, target: u64, tolerance: u64, cap: u64) -> u64 {
    if target > emitted + tolerance {
        (target - emitted).min(cap)
    } else {
        0
    }
}

/// Longest silent gap that will be filled in one go (guards against bogus timestamps).
const MAX_GAP_SEC: u64 = 120;

impl Drop for WasapiStream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.audio_client.Stop();
        }
    }
}

fn ensure_com_mta() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
}

fn get_device_name(device: &IMMDevice) -> String {
    unsafe {
        if let Ok(store) = device.OpenPropertyStore(STGM_READ) {
            if let Ok(prop) = store.GetValue(&PKEY_Device_FriendlyName) {
                let s = prop.to_string();
                if !s.trim().is_empty() {
                    return s.trim().to_string();
                }
            }
        }
    }
    "Unknown Audio Device".to_string()
}

fn get_device_id(device: &IMMDevice) -> Option<String> {
    unsafe {
        let pwstr = device.GetId().ok()?;
        let s = pwstr.to_string().ok();
        CoTaskMemFree(Some(pwstr.0 as *const c_void));
        s
    }
}

pub fn list_audio_devices() -> Result<Vec<AudioDeviceInfo>> {
    ensure_com_mta();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| anyhow!("Failed to create MMDeviceEnumerator: {}", e))?;

        let mut devices = Vec::new();
        for (flow, label) in [(eRender, "output"), (eCapture, "input")] {
            let default_id = enumerator
                .GetDefaultAudioEndpoint(flow, eConsole)
                .ok()
                .and_then(|d| get_device_id(&d));

            if let Ok(coll) = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) {
                let count = coll.GetCount().unwrap_or(0);
                for i in 0..count {
                    if let Ok(dev) = coll.Item(i) {
                        let id = get_device_id(&dev);
                        let is_default = default_id.is_some() && id == default_id;
                        devices.push(AudioDeviceInfo {
                            name: get_device_name(&dev),
                            kind: label.to_string(),
                            is_default,
                        });
                    }
                }
            }
        }
        Ok(devices)
    }
}

impl WasapiStream {
    pub fn open(kind: StreamKind) -> Result<Self> {
        ensure_com_mta();
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| anyhow!("MMDeviceEnumerator failed: {}", e))?;

            let flow: EDataFlow = match kind {
                StreamKind::Loopback => eRender,
                StreamKind::Mic => eCapture,
            };

            let device = enumerator
                .GetDefaultAudioEndpoint(flow, eConsole)
                .map_err(|e| {
                    anyhow!(
                        "No active default {} audio endpoint found: {}",
                        match kind {
                            StreamKind::Loopback => "output (loopback)",
                            StreamKind::Mic => "microphone",
                        },
                        e
                    )
                })?;

            let device_name = get_device_name(&device);

            let audio_client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| anyhow!("IMMDevice::Activate(IAudioClient) failed: {}", e))?;

            let mix_ptr: *mut WAVEFORMATEX = audio_client
                .GetMixFormat()
                .map_err(|e| anyhow!("IAudioClient::GetMixFormat failed: {}", e))?;
            if mix_ptr.is_null() {
                bail!("GetMixFormat returned null.");
            }

            let mix = ptr::read_unaligned(mix_ptr);
            let sample_rate = mix.nSamplesPerSec;
            let channels = mix.nChannels.max(1);
            let bits = mix.wBitsPerSample;
            let tag = mix.wFormatTag;

            let encoding = if tag == WAVE_FORMAT_IEEE_FLOAT && bits == 32 {
                SampleEncoding::Float32
            } else if tag == WAVE_FORMAT_PCM && bits == 16 {
                SampleEncoding::Pcm16
            } else if tag == WAVE_FORMAT_PCM && bits == 32 {
                SampleEncoding::Pcm32
            } else if tag == WAVE_FORMAT_EXTENSIBLE {
                let ext = ptr::read_unaligned(mix_ptr as *const WAVEFORMATEXTENSIBLE);
                let sub = ext.SubFormat;
                if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32 {
                    SampleEncoding::Float32
                } else if sub == KSDATAFORMAT_SUBTYPE_PCM && bits == 16 {
                    SampleEncoding::Pcm16
                } else if sub == KSDATAFORMAT_SUBTYPE_PCM && bits == 32 {
                    SampleEncoding::Pcm32
                } else {
                    CoTaskMemFree(Some(mix_ptr as *const c_void));
                    bail!(
                        "Unsupported extensible WASAPI format: bits={} subformat={:?}",
                        bits,
                        sub
                    );
                }
            } else {
                CoTaskMemFree(Some(mix_ptr as *const c_void));
                bail!("Unsupported WASAPI format tag={} bits={}", tag, bits);
            };

            let stream_flags = match kind {
                StreamKind::Loopback => AUDCLNT_STREAMFLAGS_LOOPBACK,
                StreamKind::Mic => 0,
            };

            // 1.0 second shared-mode buffer (in 100-nanosecond units)
            let init_res = audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                stream_flags,
                10_000_000,
                0,
                mix_ptr,
                None,
            );
            CoTaskMemFree(Some(mix_ptr as *const c_void));
            init_res.map_err(|e| anyhow!("IAudioClient::Initialize failed: {}", e))?;

            let capture_client: IAudioCaptureClient = audio_client.GetService().map_err(|e| {
                anyhow!(
                    "IAudioClient::GetService(IAudioCaptureClient) failed: {}",
                    e
                )
            })?;

            audio_client
                .Start()
                .map_err(|e| anyhow!("IAudioClient::Start failed: {}", e))?;

            Ok(Self {
                audio_client,
                capture_client,
                sample_rate,
                channels,
                encoding,
                device_name,
                start_100ns: qpc_now_100ns(),
                emitted: Cell::new(0),
            })
        }
    }

    /// Frame index (at `sample_rate`) corresponding to a QPC time in 100 ns units.
    fn frame_index_at(&self, qpc_100ns: u64) -> u64 {
        ((qpc_100ns.saturating_sub(self.start_100ns) as u128) * self.sample_rate as u128
            / 10_000_000) as u64
    }

    /// Appends silence so the emitted stream reaches `target` frames, if it is behind by more
    /// than `tolerance` frames. WASAPI loopback delivers *no packets at all* while nothing is
    /// playing, so without this the silent parts would vanish from the timeline.
    fn pad_silence_to(&self, out_mono: &mut Vec<f32>, target: u64, tolerance: u64) {
        let emitted = self.emitted.get();
        let gap = silence_gap(
            emitted,
            target,
            tolerance,
            self.sample_rate as u64 * MAX_GAP_SEC,
        );
        if gap > 0 {
            out_mono.resize(out_mono.len() + gap as usize, 0.0);
            self.emitted.set(emitted + gap);
        }
    }

    /// Fills the silence between the last delivered packet and now (minus a small latency
    /// margin for packets still in flight). Call before closing a clip or chunk.
    pub fn pad_to_now(&self, out_mono: &mut Vec<f32>) {
        let margin = self.sample_rate as u64 * 30 / 1000; // 30 ms
        let target = self.frame_index_at(qpc_now_100ns()).saturating_sub(margin);
        self.pad_silence_to(out_mono, target, self.sample_rate as u64 / 100);
    }

    /// Drains all currently queued audio frames from WASAPI into `out_mono` at native `sample_rate`,
    /// inserting silence for any gap in the packet timestamps.
    pub fn drain_mono(&self, out_mono: &mut Vec<f32>) -> Result<usize> {
        let mut frames_read = 0usize;
        unsafe {
            loop {
                let packet_len = self
                    .capture_client
                    .GetNextPacketSize()
                    .map_err(|e| anyhow!("GetNextPacketSize failed: {}", e))?;
                if packet_len == 0 {
                    break;
                }

                let mut data_ptr: *mut u8 = ptr::null_mut();
                let mut num_frames: u32 = 0;
                let mut flags: u32 = 0;
                let mut qpc_pos: u64 = 0;
                self.capture_client
                    .GetBuffer(
                        &mut data_ptr,
                        &mut num_frames,
                        &mut flags,
                        None,
                        Some(&mut qpc_pos),
                    )
                    .map_err(|e| anyhow!("GetBuffer failed: {}", e))?;

                let n = num_frames as usize;
                if qpc_pos > 0 {
                    let tolerance = self.sample_rate as u64 / 200; // 5 ms of jitter
                    self.pad_silence_to(out_mono, self.frame_index_at(qpc_pos), tolerance);
                }
                self.emitted.set(self.emitted.get() + n as u64);
                let is_silent =
                    (flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)) != 0 || data_ptr.is_null();

                if is_silent {
                    out_mono.resize(out_mono.len() + n, 0.0);
                } else {
                    let ch = self.channels as usize;
                    match self.encoding {
                        SampleEncoding::Float32 => {
                            let slice = std::slice::from_raw_parts(data_ptr as *const f32, n * ch);
                            for frame in slice.chunks_exact(ch) {
                                let sum: f32 = frame.iter().copied().sum();
                                out_mono.push(sum / ch as f32);
                            }
                        }
                        SampleEncoding::Pcm16 => {
                            let slice = std::slice::from_raw_parts(data_ptr as *const i16, n * ch);
                            for frame in slice.chunks_exact(ch) {
                                let sum: f32 = frame.iter().map(|&s| s as f32 / 32768.0).sum();
                                out_mono.push(sum / ch as f32);
                            }
                        }
                        SampleEncoding::Pcm32 => {
                            let slice = std::slice::from_raw_parts(data_ptr as *const i32, n * ch);
                            for frame in slice.chunks_exact(ch) {
                                let sum: f32 =
                                    frame.iter().map(|&s| s as f32 / 2_147_483_648.0).sum();
                                out_mono.push(sum / ch as f32);
                            }
                        }
                    }
                }

                self.capture_client
                    .ReleaseBuffer(num_frames)
                    .map_err(|e| anyhow!("ReleaseBuffer failed: {}", e))?;
                frames_read += n;
            }
        }
        Ok(frames_read)
    }
}

#[derive(Debug, Clone)]
pub struct ChannelAudio {
    pub device_name: String,
    pub samples_16k: Vec<f32>,
}

/// Captures a single WASAPI channel for `duration_sec` and resamples to 16 kHz mono f32.
pub fn capture_single_channel(
    kind: StreamKind,
    duration_sec: f32,
    stop: Option<&Arc<AtomicBool>>,
) -> Result<ChannelAudio> {
    let stream = WasapiStream::open(kind)?;
    let target_dur = Duration::from_secs_f32(duration_sec.clamp(0.2, 60.0));
    let t0 = Instant::now();
    let mut native_mono = Vec::with_capacity((stream.sample_rate as f32 * duration_sec) as usize);

    while t0.elapsed() < target_dur {
        if let Some(flag) = stop {
            if flag.load(Ordering::Relaxed) {
                break;
            }
        }
        let _ = stream.drain_mono(&mut native_mono)?;
        thread::sleep(Duration::from_millis(15));
    }
    let _ = stream.drain_mono(&mut native_mono);
    stream.pad_to_now(&mut native_mono);

    let samples_16k = resample_mono(&native_mono, stream.sample_rate, TARGET_SAMPLE_RATE);
    Ok(ChannelAudio {
        device_name: stream.device_name.clone(),
        samples_16k,
    })
}

#[derive(Debug, Clone)]
pub struct DualCaptureResult {
    pub system: Option<ChannelAudio>,
    pub mic: Option<ChannelAudio>,
    pub warnings: Vec<String>,
}

/// Captures system loopback, microphone, or both simultaneously in parallel threads.
pub fn capture_audio(source: AudioSource, duration_sec: f32) -> Result<DualCaptureResult> {
    match source {
        AudioSource::System => {
            let sys = capture_single_channel(StreamKind::Loopback, duration_sec, None)?;
            Ok(DualCaptureResult {
                system: Some(sys),
                mic: None,
                warnings: Vec::new(),
            })
        }
        AudioSource::Mic => {
            let mic = capture_single_channel(StreamKind::Mic, duration_sec, None)?;
            Ok(DualCaptureResult {
                system: None,
                mic: Some(mic),
                warnings: Vec::new(),
            })
        }
        AudioSource::Both => {
            let sys_handle = thread::spawn(move || {
                capture_single_channel(StreamKind::Loopback, duration_sec, None)
            });
            let mic_handle =
                thread::spawn(move || capture_single_channel(StreamKind::Mic, duration_sec, None));

            let sys_res = sys_handle
                .join()
                .map_err(|_| anyhow!("System loopback capture thread panicked"))?;
            let mic_res = mic_handle
                .join()
                .map_err(|_| anyhow!("Microphone capture thread panicked"))?;

            let mut warnings = Vec::new();
            let system = match sys_res {
                Ok(c) => Some(c),
                Err(e) => {
                    warnings.push(format!("system loopback unavailable: {}", e));
                    None
                }
            };
            let mic = match mic_res {
                Ok(c) => Some(c),
                Err(e) => {
                    warnings.push(format!("microphone unavailable: {}", e));
                    None
                }
            };

            if system.is_none() && mic.is_none() {
                bail!("Both audio sources failed: {}", warnings.join("; "));
            }

            Ok(DualCaptureResult {
                system,
                mic,
                warnings,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::silence_gap;

    #[test]
    fn loopback_gaps_are_filled_but_jitter_and_bogus_jumps_are_not() {
        let rate = 48_000;
        // 1.5 s with no packets (nothing playing) -> 1.5 s of silence
        assert_eq!(
            silence_gap(rate, rate * 5 / 2, rate / 200, rate * 120),
            rate * 3 / 2
        );
        // 3 ms of timestamp jitter -> nothing inserted
        assert_eq!(silence_gap(rate, rate + 144, rate / 200, rate * 120), 0);
        // packet stamped earlier than what was already emitted -> nothing inserted
        assert_eq!(silence_gap(rate * 2, rate, rate / 200, rate * 120), 0);
        // absurd jump is capped
        assert_eq!(silence_gap(0, rate * 10_000, 0, rate * 120), rate * 120);
    }
}
