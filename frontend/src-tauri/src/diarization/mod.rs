//! Local speaker diarization (who spoke when) for post-meeting retranscription.
//!
//! - `sortformer`: the NVIDIA Nemotron-3 Diarization model on ONNX Runtime
//! - `engine`: model download/status and whole-recording inference
//! - `assign`: attaching speaker labels to word- or segment-timed transcripts
//! - `commands`: the Tauri surface for the model file

pub mod assign;
pub mod commands;
pub mod engine;
pub mod sortformer;

pub use assign::{SpeakerPiece, SpeakerSpan, TimedWord};
