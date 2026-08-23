// Re-export all audio components
mod device;
mod recorder;
mod utils;
mod visualizer;

// Shared with the CLI binary, so these live in handy-core.
pub use device::{list_input_devices, list_output_devices, CpalDeviceInfo};
pub use handy_core::audio::{decode_to_16k_mono, decode_wav_reader, DecodedAudio, FrameResampler};
pub use recorder::{
    is_microphone_access_denied, is_no_input_device_error, AudioRecorder, VadPolicy,
};
pub use utils::{read_wav_samples, save_wav_file, verify_wav_file};
pub use visualizer::AudioVisualiser;
