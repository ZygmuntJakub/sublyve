//! Per-layer decode worker thread.
//!
//! One OS thread per loaded `Layer`. Owns the `Decoder` outright; pushes
//! decoded `VideoFrame`s into a small bounded SPSC channel that the main
//! thread (Layer::tick) drains and uploads to the GPU. Audio samples are
//! resampled inside the decoder and pumped directly into the per-layer
//! cpal ring buffer (the worker becomes the producer side of that
//! pre-existing SPSC ringbuf), so the audio path stays lock-free and
//! never crosses the main thread.
//!
//! Backpressure: the bounded frame channel (capacity 3) blocks the
//! worker when the main thread isn't draining — paused / stopped layers
//! cost nothing on the worker side (the OS schedules it off-CPU on the
//! `send` block).
//!
//! Seek epoch: each enqueued frame carries a `u64` epoch tag. Main bumps
//! its epoch on every seek and drops popped frames whose epoch is
//! older — eliminates the race where a pre-seek frame is still in the
//! channel when the seek command is observed by the worker.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};

use avengine_core::VideoFrame;
use avengine_playback::Decoder;
use ringbuf::traits::{Observer, Producer};
use tracing::warn;

use crate::audio::AudioLayerProducer;

/// Bounded frame channel capacity. ~50–100 ms of decode lookahead at
/// 30 fps; small enough that pause-resume latency is unnoticeable, big
/// enough to absorb decode jitter spikes.
pub const FRAME_CHANNEL_CAPACITY: usize = 3;

/// Commands sent from `Layer` (main thread) to the worker.
pub enum DecoderCmd {
    /// Seek to `secs` and tag subsequent frames with `epoch`.
    Seek(f64, u64),
    /// Replace the layer's audio producer. Used by audio device switch:
    /// the engine builds new ring buffers, hands the new producer side
    /// to the layer, and the layer forwards it to its running worker so
    /// the next decoded audio block lands in the new ring buffer.
    SwapAudioProducer(Option<AudioLayerProducer>),
    /// Exit the worker thread cleanly. Worker returns ownership of the
    /// audio producer (if it had one) via the JoinHandle.
    Stop,
}

/// One unit of work pushed by the worker onto the frame channel.
pub struct DecodedItem {
    /// `Some` for a normal frame, `None` for an EOF marker (worker had
    /// nothing more to decode and looping was off).
    pub frame: Option<VideoFrame>,
    /// Seek-epoch tag. Main thread drops items with `epoch < current`.
    pub epoch: u64,
}

/// Spawn the worker thread. The returned `JoinHandle` resolves to the
/// audio producer the worker owned (so the Layer can park it back onto
/// its `AudioLayerHandle` for the next clip load).
pub fn spawn(
    decoder: Decoder,
    frame_tx: SyncSender<DecodedItem>,
    cmd_rx: Receiver<DecoderCmd>,
    looping: Arc<AtomicBool>,
    audio_producer: Option<AudioLayerProducer>,
) -> JoinHandle<Option<AudioLayerProducer>> {
    thread::Builder::new()
        .name("avengine-decode".into())
        .spawn(move || worker_loop(decoder, frame_tx, cmd_rx, looping, audio_producer))
        .expect("spawn decode worker thread")
}

fn worker_loop(
    mut decoder: Decoder,
    frame_tx: SyncSender<DecodedItem>,
    cmd_rx: Receiver<DecoderCmd>,
    looping: Arc<AtomicBool>,
    mut audio_producer: Option<AudioLayerProducer>,
) -> Option<AudioLayerProducer> {
    let mut epoch: u64 = 0;
    let mut audio_scratch: Vec<f32> = Vec::new();

    loop {
        // Drain pending commands first (non-blocking).
        loop {
            match cmd_rx.try_recv() {
                Ok(DecoderCmd::Seek(secs, e)) => {
                    if let Err(err) = decoder.seek(secs) {
                        warn!("decode worker: seek failed: {err}");
                    }
                    epoch = e;
                }
                Ok(DecoderCmd::SwapAudioProducer(new_prod)) => {
                    audio_producer = new_prod;
                }
                Ok(DecoderCmd::Stop) => return audio_producer,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return audio_producer,
            }
        }

        match decoder.next_frame() {
            Ok(Some(frame)) => {
                if let Some(prod) = audio_producer.as_mut() {
                    drain_audio(&mut decoder, prod, &mut audio_scratch);
                }
                if frame_tx
                    .send(DecodedItem { frame: Some(frame), epoch })
                    .is_err()
                {
                    return audio_producer;
                }
            }
            Ok(None) => {
                if looping.load(Ordering::Relaxed) {
                    if let Err(err) = decoder.seek(0.0) {
                        warn!("decode worker: loop-seek failed: {err}");
                        return audio_producer;
                    }
                    // Same clip from the top — no epoch bump.
                } else {
                    // Notify main once that the stream ended at this epoch.
                    if frame_tx
                        .send(DecodedItem { frame: None, epoch })
                        .is_err()
                    {
                        return audio_producer;
                    }
                    // Park on the command channel; the next Seek (e.g.
                    // user scrubs back, or looping flips on and Layer
                    // sends a Seek(0)) wakes us. Stop / disconnect ends.
                    match cmd_rx.recv() {
                        Ok(DecoderCmd::Seek(secs, e)) => {
                            if let Err(err) = decoder.seek(secs) {
                                warn!("decode worker: post-EOF seek failed: {err}");
                                return audio_producer;
                            }
                            epoch = e;
                        }
                        Ok(DecoderCmd::SwapAudioProducer(new_prod)) => {
                            audio_producer = new_prod;
                        }
                        Ok(DecoderCmd::Stop) | Err(_) => return audio_producer,
                    }
                }
            }
            Err(err) => {
                warn!("decode worker: next_frame failed: {err}");
                return audio_producer;
            }
        }
    }
}

/// Drain the decoder's pending audio into the SPSC ring buffer. Mirrors
/// the pre-threading `Layer::flush_audio_to_ring`.
fn drain_audio(decoder: &mut Decoder, prod: &mut AudioLayerProducer, scratch: &mut Vec<f32>) {
    if decoder.audio_config().is_none() {
        return;
    }
    let free = prod.vacant_len();
    if free == 0 {
        return;
    }
    if scratch.len() < free {
        scratch.resize(free, 0.0);
    }
    let n = decoder.take_audio_into(&mut scratch[..free]);
    if n > 0 {
        prod.push_slice(&scratch[..n]);
    }
}
