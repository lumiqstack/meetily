// audio/transcription/pcm.rs
//
// Shared PCM conversion for remote transcription providers. Batch providers
// wrap the samples in a WAV container for multipart upload; the Gemini Live
// WebSocket sends the same little-endian 16-bit samples as bare binary frames.

/// Convert 32-bit float samples to little-endian 16-bit PCM.
///
/// Samples are clamped to [-1.0, 1.0] before scaling so that inter-sample
/// peaks above full scale wrap to the rail instead of overflowing.
pub fn pcm16_le_bytes(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        let clamped = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&clamped.to_le_bytes());
    }
    out
}

/// Encode mono f32 samples as a 16-bit PCM WAV file in memory.
pub fn encode_wav_pcm16(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data = pcm16_le_bytes(samples);
    let data_size = data.len() as u32;
    let byte_rate = sample_rate * 2;

    let mut wav = Vec::with_capacity(44 + data.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    wav.extend_from_slice(&data);

    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_encodes_to_zero_samples() {
        assert_eq!(pcm16_le_bytes(&[0.0, 0.0]), vec![0, 0, 0, 0]);
    }

    #[test]
    fn full_scale_hits_the_rails() {
        let bytes = pcm16_le_bytes(&[1.0, -1.0]);
        assert_eq!(i16::from_le_bytes([bytes[0], bytes[1]]), 32767);
        assert_eq!(i16::from_le_bytes([bytes[2], bytes[3]]), -32767);
    }

    #[test]
    fn out_of_range_samples_clamp_instead_of_wrapping() {
        // Without the clamp these would wrap to large negative values and
        // produce audible clicks in the stream sent to the gateway.
        let bytes = pcm16_le_bytes(&[9.0, -9.0]);
        assert_eq!(i16::from_le_bytes([bytes[0], bytes[1]]), 32767);
        assert_eq!(i16::from_le_bytes([bytes[2], bytes[3]]), -32767);
    }

    #[test]
    fn two_bytes_per_sample() {
        assert_eq!(pcm16_le_bytes(&[0.1; 480]).len(), 960);
    }

    #[test]
    fn wav_header_is_44_bytes_and_declares_the_payload() {
        let wav = encode_wav_pcm16(&[0.0; 160], 16000);
        assert_eq!(wav.len(), 44 + 320);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]), 320);
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            16000
        );
    }
}
