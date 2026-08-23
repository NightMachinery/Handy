//! Audio decoding shared by the app and the CLI.
//!
//! The CLI decodes files itself rather than handing paths to the app, so this
//! has to live somewhere both can reach. See `decode` for why that matters.

mod decode;
mod resampler;

pub use decode::{decode_to_16k_mono, decode_wav_reader, DecodedAudio};
pub use resampler::FrameResampler;
