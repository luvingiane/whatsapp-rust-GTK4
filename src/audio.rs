//! Voice-note recording via GStreamer: captures the microphone and encodes to
//! OGG/Opus — the format WhatsApp uses for push-to-talk notes. Records to a temp
//! file; [`Recorder::stop`] finalises it and returns the bytes plus the duration.
//!
//! The pipeline runs on the GLib main loop (GStreamer integrates with it). `stop`
//! flushes the muxer with an EOS and briefly waits for it, so the file is complete
//! before we read it back.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{anyhow, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

pub struct Recorder {
    pipeline: gst::Pipeline,
    path: PathBuf,
    started: Instant,
}

impl Recorder {
    /// Starts recording to a fresh temp `.ogg` file. Initialises GStreamer on
    /// first use (idempotent).
    pub fn start() -> Result<Self> {
        gst::init().map_err(|e| anyhow!("gstreamer init: {e}"))?;

        let path = std::env::temp_dir().join(format!(
            "wrg-voice-{}-{}.ogg",
            std::process::id(),
            now_millis()
        ));
        // Mono is plenty for a voice note and matches WhatsApp's PTT encoding.
        let desc = format!(
            "autoaudiosrc ! audioconvert ! audioresample ! audio/x-raw,channels=1 ! \
             opusenc ! oggmux ! filesink location=\"{}\"",
            path.display()
        );
        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| anyhow!("build pipeline: {e}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("parsed element is not a pipeline"))?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| anyhow!("start recording: {e}"))?;

        Ok(Self {
            pipeline,
            path,
            started: Instant::now(),
        })
    }

    /// Stops recording, finalises the OGG, and returns `(ogg_bytes, seconds)`.
    /// Consumes the recorder; the temp file is removed.
    pub fn stop(self) -> Result<(Vec<u8>, u32)> {
        let secs = (self.started.elapsed().as_secs() as u32).max(1);

        // EOS lets oggmux write its trailer; wait briefly so the file is complete.
        self.pipeline.send_event(gst::event::Eos::new());
        if let Some(bus) = self.pipeline.bus() {
            let _ = bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(3),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            );
        }
        let _ = self.pipeline.set_state(gst::State::Null);

        let bytes = std::fs::read(&self.path).map_err(|e| anyhow!("read temp ogg: {e}"))?;
        let _ = std::fs::remove_file(&self.path);
        if bytes.is_empty() {
            return Err(anyhow!("empty recording"));
        }
        Ok((bytes, secs))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Make sure the pipeline is torn down even if `stop` was never called.
        let _ = self.pipeline.set_state(gst::State::Null);
        let _ = std::fs::remove_file(&self.path);
    }
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
