//! Port of `server/lib/dictation/audio.js` — PCM16 helpers: format parsing,
//! peak detection for silence gating, WAV wrapping, and the streaming
//! linear-interpolation resampler that carries one sample across chunk
//! boundaries. (`pcm16leToFloat32` serves only the sherpa-onnx native path
//! and is not ported — see `local.rs`.)

/// `parsePcmRateFromFormat`: pull `rate=<n>` out of
/// `"audio/pcm;rate=16000;bits=16"` (case-insensitive, `rate` must be a
/// whole `;`/`,`/whitespace-delimited token, digits terminated likewise).
pub fn parse_pcm_rate_from_format(format: &str, fallback: Option<u32>) -> Option<u32> {
    let text = format;
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < text.len() {
        // Find the next occurrence of "rate" as a standalone token.
        let Some(start) = text[index..].to_ascii_lowercase().find("rate") else {
            return fallback;
        };
        let at = index + start;
        let before_ok =
            at == 0 || matches!(bytes[at - 1], b';' | b',' | b' ' | b'\t' | b'\n' | b'\r');
        if before_ok {
            let mut cursor = at + 4;
            while cursor < text.len() && matches!(bytes[cursor], b' ' | b'\t') {
                cursor += 1;
            }
            if cursor < text.len() && bytes[cursor] == b'=' {
                cursor += 1;
                while cursor < text.len() && matches!(bytes[cursor], b' ' | b'\t') {
                    cursor += 1;
                }
                let digits_start = cursor;
                while cursor < text.len() && bytes[cursor].is_ascii_digit() {
                    cursor += 1;
                }
                let after_ok = cursor == text.len()
                    || matches!(bytes[cursor], b';' | b',' | b' ' | b'\t' | b'\n' | b'\r');
                if cursor > digits_start && after_ok {
                    let rate = text[digits_start..cursor]
                        .parse::<u32>()
                        .ok()
                        .filter(|rate| *rate > 0);
                    return match rate {
                        Some(rate) => Some(rate),
                        None => fallback,
                    };
                }
            }
        }
        index = at + 4;
    }
    fallback
}

/// `pcm16lePeakAbs`: peak absolute sample; early-exits at full scale.
/// Returns an error for odd-length buffers, like the JS throw.
pub fn pcm16le_peak_abs(pcm16le: &[u8]) -> Result<i32, String> {
    if pcm16le.is_empty() {
        return Ok(0);
    }
    if !pcm16le.len().is_multiple_of(2) {
        return Err(format!(
            "PCM16 chunk byteLength must be even, got {}",
            pcm16le.len()
        ));
    }
    let mut peak: i32 = 0;
    for chunk in pcm16le.chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
        let abs = (sample as i32).abs();
        if abs > peak {
            peak = abs;
            if peak >= 32767 {
                break;
            }
        }
    }
    Ok(peak)
}

/// `pcm16ToWav`: wrap raw PCM16LE mono audio in a 44-byte-header WAV
/// container.
pub fn pcm16_to_wav(pcm_buffer: &[u8], sample_rate: u32) -> Vec<u8> {
    let channels: u32 = 1;
    let bits_per_sample: u32 = 16;
    let mut wav = Vec::with_capacity(44 + pcm_buffer.len());
    let byte_rate = (sample_rate * channels * bits_per_sample) / 8;
    let block_align = (channels * bits_per_sample) / 8;

    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36u32 + pcm_buffer.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&(channels as u16).to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&(block_align as u16).to_le_bytes());
    wav.extend_from_slice(&(bits_per_sample as u16).to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm_buffer.len() as u32).to_le_bytes());
    wav.extend_from_slice(pcm_buffer);
    wav
}

/// Streaming linear-interpolation resampler for PCM16LE mono audio
/// (`Pcm16MonoResampler`). Math follows the JS exactly: source samples are
/// stored as f32 (`sample / 32768`), positions advance in f64, and output
/// samples are `round(clamp(interp, -1, 1) * 32767)`.
pub struct Pcm16MonoResampler {
    step: f64,
    pos: f64,
    carry_sample: Option<i16>,
}

impl Pcm16MonoResampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            step: input_rate as f64 / output_rate as f64,
            pos: 0.0,
            carry_sample: None,
        }
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.carry_sample = None;
    }

    pub fn process_chunk(&mut self, pcm16le: &[u8]) -> Result<Vec<u8>, String> {
        if pcm16le.is_empty() {
            return Ok(Vec::new());
        }
        if !pcm16le.len().is_multiple_of(2) {
            return Err(format!(
                "PCM16 chunk byteLength must be even, got {}",
                pcm16le.len()
            ));
        }
        let src_chunk: Vec<i16> = pcm16le
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();

        let has_carry = self.carry_sample.is_some();
        let src_len = src_chunk.len() + usize::from(has_carry);
        if src_len < 2 {
            self.carry_sample = src_chunk.last().copied().or(self.carry_sample);
            return Ok(Vec::new());
        }

        // f32 storage mirrors the JS Float32Array; interpolation runs in f64
        // (JS numbers).
        let mut src = vec![0.0f32; src_len];
        let mut offset = 0usize;
        if let Some(carry) = self.carry_sample {
            src[0] = carry as f32 / 32768.0;
            offset = 1;
        }
        for (index, sample) in src_chunk.iter().enumerate() {
            src[offset + index] = *sample as f32 / 32768.0;
        }

        let mut out: Vec<i16> = Vec::new();
        let max_pos = (src.len() - 1) as f64;
        while self.pos < max_pos {
            let i = self.pos.floor();
            let frac = self.pos - i;
            let index = i as usize;
            let s0 = src[index] as f64;
            let s1 = src[index + 1] as f64;
            let sample = s0 + (s1 - s0) * frac;
            let clamped = sample.clamp(-1.0, 1.0);
            out.push((clamped * 32767.0).round() as i16);
            self.pos += self.step;
        }

        self.carry_sample = src_chunk.last().copied();

        let shift = (src.len() - 1) as f64;
        self.pos -= shift;
        if self.pos < 0.0 {
            self.pos = 0.0;
        }

        let mut bytes = Vec::with_capacity(out.len() * 2);
        for sample in out {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rates_with_delimiters() {
        assert_eq!(
            parse_pcm_rate_from_format("audio/pcm;rate=16000;bits=16", None),
            Some(16000)
        );
        assert_eq!(parse_pcm_rate_from_format("RATE = 8000", None), Some(8000));
        assert_eq!(
            parse_pcm_rate_from_format("audio/pcm; bitrate=16000", None),
            None
        );
        // `rate` must be a whole token: `generate=1` does not match.
        assert_eq!(parse_pcm_rate_from_format("generate=1", None), None);
        assert_eq!(
            parse_pcm_rate_from_format("audio/pcm,rate=44100,x=1", None),
            Some(44100)
        );
        assert_eq!(parse_pcm_rate_from_format("rate=0", Some(1234)), Some(1234));
        assert_eq!(parse_pcm_rate_from_format("", Some(48000)), Some(48000));
    }

    #[test]
    fn peak_abs_finds_the_loudest_sample() {
        let samples = [100i16, -5000, 30, 0];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(pcm16le_peak_abs(&bytes).unwrap(), 5000);
        assert_eq!(pcm16le_peak_abs(&[]).unwrap(), 0);
        assert!(pcm16le_peak_abs(&[1, 2, 3]).is_err());
        // Full scale early-exits but still reports 32767.
        let full = [32767i16, 0];
        let bytes: Vec<u8> = full.iter().flat_map(|s| s.to_le_bytes()).collect();
        assert_eq!(pcm16le_peak_abs(&bytes).unwrap(), 32767);
    }

    #[test]
    fn wav_header_has_the_expected_fields() {
        let pcm = vec![0u8, 0, 1, 0];
        let wav = pcm16_to_wav(&pcm, 16000);
        assert_eq!(wav.len(), 48);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 40);
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 4);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16000);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
    }

    fn pcm(bytes: &[i16]) -> Vec<u8> {
        bytes.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn passthrough_when_rates_match_has_no_gain() {
        // inputRate == outputRate → the manager never constructs a resampler;
        // but the step-1 resampler must also be sample-preserving-ish.
        let mut resampler = Pcm16MonoResampler::new(16000, 16000);
        let out = resampler.process_chunk(&pcm(&[1000, -2000, 3000])).unwrap();
        let samples: Vec<i16> = out
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(samples, vec![1000, -2000]);
    }

    #[test]
    fn upsampling_and_downsampling_stay_continuous() {
        // 16000 → 48000: ~3 output samples per input sample (the exact count
        // follows the f64 position arithmetic, like the JS).
        let mut up = Pcm16MonoResampler::new(16000, 48000);
        let out = up.process_chunk(&pcm(&[1000; 100])).unwrap();
        let ups = out.len() / 2;
        assert!((290..=305).contains(&ups), "upsampled {ups}");

        // 48000 → 16000: ~1/3.
        let mut down = Pcm16MonoResampler::new(48000, 16000);
        let out = down.process_chunk(&pcm(&[-5000; 300])).unwrap();
        let downs = out.len() / 2;
        assert!((95..=105).contains(&downs), "downsampled {downs}");

        // Reset clears the carry so streams restart cleanly.
        down.reset();
        assert_eq!(down.pos, 0.0);
    }

    #[test]
    fn resampler_rejects_odd_chunks() {
        let mut resampler = Pcm16MonoResampler::new(16000, 48000);
        assert!(resampler.process_chunk(&[1, 2, 3]).is_err());
    }
}
