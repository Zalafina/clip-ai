//! In-memory synthesized PCM WAV audio cues and non-blocking WinMM playback.

use crate::config::CustomSoundPaths;
use crate::win_util::to_wide;
use std::f32::consts::PI;
use std::fs;
use std::sync::{Arc, RwLock};
use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

const SAMPLE_RATE: u32 = 44_100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundCue {
    Start,
    Processing,
    Complete,
    Error,
    Busy,
}

#[derive(Default)]
struct SoundBuffers {
    start: Vec<u8>,
    processing: Vec<u8>,
    complete: Vec<u8>,
    error: Vec<u8>,
    busy: Vec<u8>,
}

/// Thread-safe audio manager holding pre-synthesized or custom WAV buffers in memory.
/// Buffers stay alive across async `PlaySoundW` playback so memory pointers remain valid.
#[derive(Clone)]
pub struct AudioPlayer {
    inner: Arc<RwLock<AudioState>>,
}

struct AudioState {
    enabled: bool,
    buffers: SoundBuffers,
    // Keep previous buffer alive briefly when swapping settings during async playback.
    _retired: Vec<Vec<u8>>,
}

impl AudioPlayer {
    pub fn new(enabled: bool, volume: u8, custom: &CustomSoundPaths) -> Self {
        Self {
            inner: Arc::new(RwLock::new(AudioState {
                enabled,
                buffers: build_buffers(volume, custom),
                _retired: Vec::new(),
            })),
        }
    }

    pub fn reload(&self, enabled: bool, volume: u8, custom: &CustomSoundPaths) {
        let new_buffers = build_buffers(volume, custom);
        if let Ok(mut guard) = self.inner.write() {
            guard.enabled = enabled;
            let old = std::mem::replace(&mut guard.buffers, new_buffers);
            guard._retired = vec![old.start, old.processing, old.complete, old.error, old.busy];
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        if let Ok(mut guard) = self.inner.write() {
            guard.enabled = enabled;
        }
    }

    pub fn play(&self, cue: SoundCue) {
        self.play_internal(cue, false);
    }

    /// Play even if master mute is on (used for explicit "Test Sound" buttons in Settings).
    pub fn play_preview(&self, cue: SoundCue) {
        self.play_internal(cue, true);
    }

    fn play_internal(&self, cue: SoundCue, force: bool) {
        let guard = match self.inner.read() {
            Ok(g) => g,
            Err(_) => return,
        };
        if !force && !guard.enabled {
            return;
        }
        let buf = match cue {
            SoundCue::Start => &guard.buffers.start,
            SoundCue::Processing => &guard.buffers.processing,
            SoundCue::Complete => &guard.buffers.complete,
            SoundCue::Error => &guard.buffers.error,
            SoundCue::Busy => &guard.buffers.busy,
        };
        if buf.len() < 44 {
            return;
        }
        unsafe {
            PlaySoundW(
                buf.as_ptr() as *const u16,
                std::ptr::null_mut(),
                SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
            );
        }
    }
}

fn build_buffers(volume: u8, custom: &CustomSoundPaths) -> SoundBuffers {
    let gain = (volume.min(100) as f32) / 100.0;
    SoundBuffers {
        start: load_or_synth(&custom.start, &[(523.25, 65, 0.32), (659.25, 85, 0.35)], gain),
        processing: load_or_synth(&custom.processing, &[(440.0, 45, 0.16)], gain),
        complete: load_or_synth(
            &custom.complete,
            &[(523.25, 65, 0.28), (659.25, 65, 0.30), (783.99, 120, 0.36)],
            gain,
        ),
        error: load_or_synth(&custom.error, &[(329.63, 120, 0.34), (220.0, 170, 0.36)], gain),
        busy: load_or_synth(&custom.busy, &[(392.0, 40, 0.22), (392.0, 40, 0.22)], gain),
    }
}

fn load_or_synth(path: &str, notes: &[(f32, u32, f32)], gain: f32) -> Vec<u8> {
    let trimmed = path.trim();
    if !trimmed.is_empty() {
        if let Ok(bytes) = fs::read(trimmed) {
            if bytes.len() >= 44 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
                return bytes;
            }
        }
    }
    synthesize_wav(notes, gain)
}

/// Synthesize a 16-bit mono PCM WAV buffer from `(freq_hz, duration_ms, base_amp)` notes.
pub fn synthesize_wav(notes: &[(f32, u32, f32)], gain: f32) -> Vec<u8> {
    let gap_samples = (SAMPLE_RATE * 14) / 1000;
    let mut pcm: Vec<i16> = Vec::new();

    for (idx, &(freq, ms, base_amp)) in notes.iter().enumerate() {
        if idx > 0 {
            pcm.extend(std::iter::repeat_n(0i16, gap_samples as usize));
        }
        let count = ((SAMPLE_RATE * ms) / 1000).max(1) as usize;
        let fade_len = (count / 6).max(32).min(count / 2);
        let amp = (base_amp * gain).clamp(0.0, 0.95);

        for i in 0..count {
            let env = if i < fade_len {
                i as f32 / fade_len as f32
            } else if i >= count - fade_len {
                (count - i) as f32 / fade_len as f32
            } else {
                1.0
            };
            let t = i as f32 / SAMPLE_RATE as f32;
            // Pure sine with tiny 2nd harmonic warmth for pleasant non-harsh tone
            let wave = (2.0 * PI * freq * t).sin() * 0.88 + (4.0 * PI * freq * t).sin() * 0.12;
            let sample = (wave * amp * env * i16::MAX as f32).round() as i16;
            pcm.push(sample);
        }
    }

    let data_bytes = (pcm.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_bytes as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    wav.extend_from_slice(&1u16.to_le_bytes()); // Mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // Byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // Block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // Bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for s in pcm {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

/// Silence any currently playing async sound (helper for cleanup).
#[allow(dead_code)]
pub fn stop_sound() {
    let empty = to_wide("");
    let _ = empty;
    unsafe {
        PlaySoundW(std::ptr::null(), std::ptr::null_mut(), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesized_wav_has_valid_riff_header_and_scaling() {
        let loud = synthesize_wav(&[(440.0, 50, 0.5)], 1.0);
        let quiet = synthesize_wav(&[(440.0, 50, 0.5)], 0.25);
        assert_eq!(&loud[0..4], b"RIFF");
        assert_eq!(&loud[8..12], b"WAVE");
        assert_eq!(&loud[36..40], b"data");
        assert_eq!(loud.len(), quiet.len());
        let data_len = u32::from_le_bytes(loud[40..44].try_into().unwrap()) as usize;
        assert_eq!(loud.len(), 44 + data_len);

        let peak_loud = loud[44..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]).unsigned_abs())
            .max()
            .unwrap();
        let peak_quiet = quiet[44..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]).unsigned_abs())
            .max()
            .unwrap();
        assert!(peak_loud > peak_quiet * 3);
    }
}
