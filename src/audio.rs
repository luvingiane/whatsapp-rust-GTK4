//! Voice-note recording via GStreamer: captures the microphone and encodes to
//! OGG/Opus — the format WhatsApp uses for push-to-talk notes. A `level` element
//! posts RMS messages so we can show a live waveform while recording and ship a
//! 64-value amplitude waveform with the note. Records to a temp file; [`stop`]
//! finalises it and returns the bytes, duration and waveform.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{anyhow, Result};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;

/// Number of amplitude buckets in the waveform (matches WhatsApp's 64).
pub const WAVEFORM_LEN: usize = 64;

pub struct Recorder {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    path: PathBuf,
    started: Instant,
    /// Per-interval mic amplitude (0..1), collected from `level` messages.
    rms: RefCell<Vec<f64>>,
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
        // Mono OGG/Opus (WhatsApp PTT format); `level` posts RMS every 50 ms.
        let desc = format!(
            "autoaudiosrc ! audioconvert ! audioresample ! audio/x-raw,channels=1 ! \
             level interval=50000000 post-messages=true ! \
             opusenc ! oggmux ! filesink location=\"{}\"",
            path.display()
        );
        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| anyhow!("build pipeline: {e}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("parsed element is not a pipeline"))?;
        let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline has no bus"))?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| anyhow!("start recording: {e}"))?;

        Ok(Self {
            pipeline,
            bus,
            path,
            started: Instant::now(),
            rms: RefCell::new(Vec::new()),
        })
    }

    /// Drains pending `level` messages into the amplitude buffer. Call it from a
    /// UI timer to keep the live waveform fresh.
    pub fn poll_levels(&self) {
        while let Some(msg) = self.bus.pop_filtered(&[gst::MessageType::Element]) {
            if let gst::MessageView::Element(e) = msg.view() {
                if let Some(s) = e.structure() {
                    if s.name() == "level" {
                        if let Ok(arr) = s.get::<glib::ValueArray>("rms") {
                            let mut sum = 0.0;
                            let mut n = 0usize;
                            for v in arr.iter() {
                                if let Ok(db) = v.get::<f64>() {
                                    sum += db_to_amp(db);
                                    n += 1;
                                }
                            }
                            if n > 0 {
                                self.rms.borrow_mut().push(sum / n as f64);
                            }
                        }
                    }
                }
            }
        }
    }

    /// A snapshot of the amplitudes collected so far (0..1), for the live UI.
    pub fn levels(&self) -> Vec<f64> {
        self.rms.borrow().clone()
    }

    /// Seconds elapsed since recording started (for the live timer label).
    pub fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Stops recording, finalises the OGG, and returns `(ogg_bytes, seconds,
    /// waveform)` where `waveform` is `WAVEFORM_LEN` bytes scaled 0..100.
    pub fn stop(self) -> Result<(Vec<u8>, u32, Vec<u8>)> {
        self.poll_levels();
        let secs = (self.started.elapsed().as_secs() as u32).max(1);

        // EOS lets oggmux write its trailer; wait briefly so the file is complete.
        self.pipeline.send_event(gst::event::Eos::new());
        let _ = self.bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(3),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        let _ = self.pipeline.set_state(gst::State::Null);

        let waveform = resample_waveform(&self.rms.borrow(), WAVEFORM_LEN);
        let bytes = std::fs::read(&self.path).map_err(|e| anyhow!("read temp ogg: {e}"))?;
        let _ = std::fs::remove_file(&self.path);
        if bytes.is_empty() {
            return Err(anyhow!("empty recording"));
        }
        Ok((bytes, secs, waveform))
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Converts an RMS value in dBFS (≤ 0) to a linear amplitude in `0..1`.
fn db_to_amp(db: f64) -> f64 {
    if !db.is_finite() {
        return 0.0;
    }
    (10f64.powf(db / 20.0)).clamp(0.0, 1.0)
}

/// Buckets per-interval amplitudes into `n` peaks, normalized to the loudest
/// (so quiet notes still show bars), scaled to `0..100`.
fn resample_waveform(levels: &[f64], n: usize) -> Vec<u8> {
    if levels.is_empty() {
        return vec![0; n];
    }
    let max = levels.iter().cloned().fold(0.0f64, f64::max).max(1e-6);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let start = i * levels.len() / n;
        let end = (((i + 1) * levels.len() / n).max(start + 1)).min(levels.len());
        let peak = levels[start..end].iter().cloned().fold(0.0f64, f64::max);
        out.push(((peak / max) * 100.0).round().clamp(0.0, 100.0) as u8);
    }
    out
}

fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
