//! Decoding arbitrary audio files down to the 16 kHz mono `f32` buffer every
//! transcription engine expects.
//!
//! The app itself only ever writes 16 kHz mono 16-bit WAVs, so the original
//! reader ([`read_wav_samples`](super::read_wav_samples)) just pulled `i16`s and
//! divided by 32767 with no validation at all. That is fine for files Handy
//! wrote and silently wrong for everything else: a 48 kHz stereo file decodes to
//! garbage that is also played back at the wrong speed. Anything that accepts a
//! file from the user — the CLI, history retry — needs a decoder that either
//! converts correctly or says why it cannot.

use super::FrameResampler;
use crate::audio_toolkit::constants::WHISPER_SAMPLE_RATE;
use anyhow::{bail, Result};
use hound::{SampleFormat, WavReader};
use std::io::{Read, Seek};
use std::path::Path;
use std::time::Duration;

/// Mono frames accumulated before being handed to the resampler.
const DECODE_CHUNK: usize = 4096;

/// Frame size for the resampler. Matches the recording path so a file and a
/// live capture of the same audio resample identically.
const RESAMPLE_FRAME: Duration = Duration::from_millis(30);

/// Sample rates outside this range are rejected as corrupt rather than fed to
/// the resampler, which would either allocate absurdly or divide by ~zero.
const MIN_SAMPLE_RATE: u32 = 4_000;
const MAX_SAMPLE_RATE: u32 = 384_000;

/// A decoded buffer plus what it was decoded *from*, so callers can report the
/// conversion they applied.
#[derive(Debug, Clone)]
pub struct DecodedAudio {
    /// 16 kHz mono, nominally in [-1.0, 1.0].
    pub samples: Vec<f32>,
    pub source_rate: u32,
    pub source_channels: u16,
    pub source_bits: u16,
    /// Duration of the source audio, in seconds.
    pub duration_secs: f64,
}

impl DecodedAudio {
    /// Whether decoding was a pure read (no downmix, no resample).
    pub fn is_native(&self) -> bool {
        self.source_rate == WHISPER_SAMPLE_RATE && self.source_channels == 1
    }

    /// Human-readable description of the source format, for logs and errors.
    pub fn source_description(&self) -> String {
        format!(
            "{} Hz / {} ch / {}-bit",
            self.source_rate, self.source_channels, self.source_bits
        )
    }
}

/// Decode a WAV file at `path` to 16 kHz mono.
pub fn decode_to_16k_mono<P: AsRef<Path>>(path: P) -> Result<DecodedAudio> {
    let path = path.as_ref();
    let reader = WavReader::open(path)
        .map_err(|e| anyhow::anyhow!("cannot read {} as WAV: {}", path.display(), e))?;
    decode_wav_reader(reader)
        .map_err(|e| anyhow::anyhow!("failed to decode {}: {}", path.display(), e))
}

/// Decode an already-opened WAV stream to 16 kHz mono.
///
/// Handles 8/16/24/32-bit integer and 32-bit float PCM, any channel count, and
/// any sample rate in [`MIN_SAMPLE_RATE`]..=[`MAX_SAMPLE_RATE`].
pub fn decode_wav_reader<R: Read + Seek>(reader: WavReader<R>) -> Result<DecodedAudio> {
    let spec = reader.spec();

    if spec.channels == 0 {
        bail!("WAV declares zero channels");
    }
    if !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&spec.sample_rate) {
        bail!(
            "unsupported sample rate {} Hz (expected {}..={})",
            spec.sample_rate,
            MIN_SAMPLE_RATE,
            MAX_SAMPLE_RATE
        );
    }
    match spec.sample_format {
        SampleFormat::Float if spec.bits_per_sample != 32 => {
            bail!(
                "unsupported {}-bit float WAV (only 32-bit float is defined)",
                spec.bits_per_sample
            );
        }
        SampleFormat::Int if !(1..=32).contains(&spec.bits_per_sample) => {
            bail!("unsupported {}-bit integer WAV", spec.bits_per_sample);
        }
        _ => {}
    }

    let channels = spec.channels as usize;

    // Downmix must happen before resampling: FrameResampler is mono-only.
    let mut state = Downmix::new(channels, spec.sample_rate);
    match spec.sample_format {
        SampleFormat::Float => {
            for sample in reader.into_samples::<f32>() {
                state.push(sample?);
            }
        }
        SampleFormat::Int => {
            // hound sign-extends every integer width into i32, so one arm
            // covers 8/16/24/32-bit. Dividing by the largest positive value of
            // the source width matches what read_wav_samples has always done
            // for 16-bit (v / 32767.0), which keeps CLI and history transcripts
            // bit-identical for files Handy wrote itself. It must be a division,
            // not a multiply by a precomputed reciprocal — those differ in the
            // last ULP.
            let full_scale = ((1i64 << (spec.bits_per_sample - 1)) - 1) as f32;
            for sample in reader.into_samples::<i32>() {
                state.push(sample? as f32 / full_scale);
            }
        }
    }
    let (samples, frames) = state.finish();

    if frames == 0 {
        bail!("WAV contains no audio samples");
    }

    Ok(DecodedAudio {
        samples,
        source_rate: spec.sample_rate,
        source_channels: spec.channels,
        source_bits: spec.bits_per_sample,
        duration_secs: frames as f64 / spec.sample_rate as f64,
    })
}

/// Averages interleaved channels into mono and resamples to 16 kHz, streaming
/// so a long high-rate multi-channel file is never held at its source rate.
struct Downmix {
    channels: usize,
    source_rate: u32,
    resampler: Option<FrameResampler>,
    /// Accumulator for the channels of the frame currently being read.
    frame_sum: f32,
    frame_filled: usize,
    /// Mono frames waiting to be pushed through the resampler.
    chunk: Vec<f32>,
    out: Vec<f32>,
    /// Mono frames seen, used to compute the exact expected output length.
    frames: usize,
}

impl Downmix {
    fn new(channels: usize, source_rate: u32) -> Self {
        let resampler = (source_rate != WHISPER_SAMPLE_RATE).then(|| {
            FrameResampler::new(
                source_rate as usize,
                WHISPER_SAMPLE_RATE as usize,
                RESAMPLE_FRAME,
            )
        });
        Self {
            channels,
            source_rate,
            resampler,
            frame_sum: 0.0,
            frame_filled: 0,
            chunk: Vec::with_capacity(DECODE_CHUNK),
            out: Vec::new(),
            frames: 0,
        }
    }

    fn push(&mut self, value: f32) {
        self.frame_sum += value;
        self.frame_filled += 1;
        if self.frame_filled < self.channels {
            return;
        }
        // Average rather than picking channel 0: a stereo interview with one
        // speaker per channel would otherwise lose half the conversation.
        self.chunk.push(self.frame_sum / self.channels as f32);
        self.frame_sum = 0.0;
        self.frame_filled = 0;
        self.frames += 1;
        if self.chunk.len() >= DECODE_CHUNK {
            self.drain_chunk();
        }
    }

    fn drain_chunk(&mut self) {
        if self.chunk.is_empty() {
            return;
        }
        match self.resampler.as_mut() {
            Some(resampler) => {
                let out = &mut self.out;
                resampler.push(&self.chunk, |frame| out.extend_from_slice(frame));
            }
            None => self.out.extend_from_slice(&self.chunk),
        }
        self.chunk.clear();
    }

    /// Returns the 16 kHz mono buffer and the number of source frames consumed.
    ///
    /// A trailing partial frame (a file truncated mid-frame) is dropped rather
    /// than emitted at the wrong amplitude.
    fn finish(mut self) -> (Vec<f32>, usize) {
        self.drain_chunk();
        if let Some(resampler) = self.resampler.as_mut() {
            let out = &mut self.out;
            resampler.finish(|frame| out.extend_from_slice(frame));
            // finish() zero-pads its final frame up to the 30 ms boundary, so
            // without this every resampled file would gain up to 30 ms of
            // trailing silence.
            let expected = (self.frames as u128 * WHISPER_SAMPLE_RATE as u128
                / self.source_rate as u128) as usize;
            self.out.truncate(expected);
        }
        (self.out, self.frames)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{WavSpec, WavWriter};
    use std::io::Cursor;

    fn write_wav(
        spec: WavSpec,
        write: impl FnOnce(&mut WavWriter<&mut Cursor<Vec<u8>>>),
    ) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = WavWriter::new(&mut cursor, spec).unwrap();
            write(&mut writer);
            writer.finalize().unwrap();
        }
        cursor.into_inner()
    }

    fn int_spec(rate: u32, channels: u16, bits: u16) -> WavSpec {
        WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: bits,
            sample_format: SampleFormat::Int,
        }
    }

    fn decode(bytes: Vec<u8>) -> DecodedAudio {
        decode_wav_reader(WavReader::new(Cursor::new(bytes)).unwrap()).unwrap()
    }

    /// A 16 kHz mono 16-bit file is what the app writes; the decoder must agree
    /// with the historical read_wav_samples convention exactly, or the same
    /// recording would transcribe differently through different code paths.
    #[test]
    fn matches_the_legacy_convention_for_native_files() {
        let values: Vec<i16> = vec![0, 1, -1, 32767, -32768, 1234, -4321];
        let bytes = write_wav(int_spec(16_000, 1, 16), |w| {
            for v in &values {
                w.write_sample(*v).unwrap();
            }
        });
        let decoded = decode(bytes);
        assert!(decoded.is_native());
        assert_eq!(decoded.samples.len(), values.len());
        for (got, want) in decoded.samples.iter().zip(&values) {
            assert_eq!(*got, *want as f32 / i16::MAX as f32);
        }
    }

    #[test]
    fn reports_the_source_format() {
        let bytes = write_wav(int_spec(44_100, 2, 24), |w| {
            for _ in 0..4410 {
                w.write_sample(0i32).unwrap();
                w.write_sample(0i32).unwrap();
            }
        });
        let decoded = decode(bytes);
        assert_eq!(decoded.source_rate, 44_100);
        assert_eq!(decoded.source_channels, 2);
        assert_eq!(decoded.source_bits, 24);
        assert!(!decoded.is_native());
        assert!((decoded.duration_secs - 0.1).abs() < 1e-9);
        assert_eq!(decoded.source_description(), "44100 Hz / 2 ch / 24-bit");
    }

    #[test]
    fn averages_channels_rather_than_taking_the_first() {
        // Left is silent, right carries the signal. Picking channel 0 would
        // return silence.
        let bytes = write_wav(int_spec(16_000, 2, 16), |w| {
            for _ in 0..100 {
                w.write_sample(0i16).unwrap();
                w.write_sample(16_000i16).unwrap();
            }
        });
        let decoded = decode(bytes);
        assert_eq!(decoded.samples.len(), 100);
        let expected = (16_000.0 / i16::MAX as f32) / 2.0;
        for s in &decoded.samples {
            assert!((s - expected).abs() < 1e-6, "got {s}, want {expected}");
        }
    }

    #[test]
    fn eight_bit_round_trips_through_hound() {
        // 8-bit WAV is stored unsigned offset-binary; this pins hound's
        // normalization so a change in that behaviour fails loudly here rather
        // than as quietly wrong audio.
        let bytes = write_wav(int_spec(16_000, 1, 8), |w| {
            for v in [0i8, 127, -128, 64] {
                w.write_sample(v as i32).unwrap();
            }
        });
        let decoded = decode(bytes);
        assert_eq!(decoded.samples.len(), 4);
        assert_eq!(decoded.samples[0], 0.0);
        assert!((decoded.samples[1] - 1.0).abs() < 1e-6);
        assert!(decoded.samples[2] < -1.0 + 1e-3);
    }

    #[test]
    fn decodes_32_bit_float() {
        let spec = WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let bytes = write_wav(spec, |w| {
            for v in [0.0f32, 0.5, -0.5, 1.0] {
                w.write_sample(v).unwrap();
            }
        });
        let decoded = decode(bytes);
        assert_eq!(decoded.samples, vec![0.0, 0.5, -0.5, 1.0]);
    }

    /// The zero-padding trap: `FrameResampler::finish()` pads its final frame
    /// out to 30 ms, so without the truncation in `Downmix::finish` every
    /// resampled file would gain up to 30 ms of trailing silence.
    ///
    /// The resampler can also fall a little *short* of the ideal length, since
    /// its FFT chunking drops a sub-chunk tail — measured at 160 samples for
    /// exactly one second of 48 kHz, and zero for 2 s and up, so it is a
    /// boundary artifact rather than accumulating drift. Both directions are
    /// bounded by one frame.
    #[test]
    fn resampled_length_is_never_padded_and_never_far_short() {
        const FRAME: usize = 480; // 30 ms at 16 kHz
        for (rate, secs) in [
            (8_000u32, 1),
            (44_100, 1),
            (48_000, 1),
            (48_000, 2),
            (48_000, 10),
        ] {
            let frames = rate as usize * secs;
            let bytes = write_wav(int_spec(rate, 1, 16), |w| {
                for _ in 0..frames {
                    w.write_sample(0i16).unwrap();
                }
            });
            let got = decode(bytes).samples.len();
            let expected = frames * WHISPER_SAMPLE_RATE as usize / rate as usize;
            assert!(
                got <= expected,
                "{rate} Hz x{secs}s: {got} samples exceeds the ideal {expected} — \
                 the padded tail is leaking through"
            );
            assert!(
                expected - got < FRAME,
                "{rate} Hz x{secs}s: {got} samples is more than one frame short of {expected}"
            );
        }
    }

    /// No accumulating drift: the shortfall must not grow with duration.
    #[test]
    fn resampling_does_not_drift_with_duration() {
        let rate = 48_000usize;
        let shortfall = |secs: usize| {
            let frames = rate * secs;
            let bytes = write_wav(int_spec(rate as u32, 1, 16), |w| {
                for _ in 0..frames {
                    w.write_sample(0i16).unwrap();
                }
            });
            let expected = frames * WHISPER_SAMPLE_RATE as usize / rate;
            expected - decode(bytes).samples.len()
        };
        assert_eq!(shortfall(2), shortfall(10));
    }

    #[test]
    fn resampling_preserves_a_constant_signal() {
        let frames = 48_000usize;
        let bytes = write_wav(int_spec(48_000, 1, 16), |w| {
            for _ in 0..frames {
                w.write_sample(8_000i16).unwrap();
            }
        });
        let decoded = decode(bytes);
        let expected = 8_000.0 / i16::MAX as f32;
        // Skip the resampler's leading transient.
        let tail = &decoded.samples[480..decoded.samples.len() - 480];
        for s in tail {
            assert!((s - expected).abs() < 0.01, "got {s}, want ~{expected}");
        }
    }

    #[test]
    fn rejects_an_empty_file() {
        let bytes = write_wav(int_spec(16_000, 1, 16), |_| {});
        let err = decode_wav_reader(WavReader::new(Cursor::new(bytes)).unwrap()).unwrap_err();
        assert!(err.to_string().contains("no audio samples"), "{err}");
    }

    #[test]
    fn single_sample_is_decodable() {
        let bytes = write_wav(int_spec(16_000, 1, 16), |w| {
            w.write_sample(1000i16).unwrap();
        });
        let decoded = decode(bytes);
        assert_eq!(decoded.samples.len(), 1);
    }
}
