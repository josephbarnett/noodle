//! Server-Sent Events (W3C SSE) codec — the L4 body-framing
//! step for the layered architecture (015 §2 L4, §11 step 3
//! precondition).
//!
//! Implements [`Codec<Input = Bytes, Output = BodyFrameEvent>`].
//! Frames are demarcated by `\n\n` (blank-line terminator).
//! Within a frame we parse `event:` and `data:` fields per the
//! W3C SSE grammar; everything else (comments starting with `:`,
//! `id:`, `retry:`) round-trips verbatim through the
//! [`FrameSource::Upstream`] discriminator's raw bytes.
//!
//! [`Codec<Input = Bytes, Output = BodyFrameEvent>`]: noodle_core::layered::Codec
//! [`FrameSource::Upstream`]: noodle_core::layered::FrameSource

mod codec;

pub use codec::{SseFrameCodec, SseFrameCodecInstance};
