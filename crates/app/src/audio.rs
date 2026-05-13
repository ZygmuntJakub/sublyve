use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use avengine_playback::AudioConfig;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Split};
use tracing::{info, warn};

/// Producer side of the per-layer SPSC ring buffer. Exposed so the
/// decode worker thread can own one directly and push samples without
/// crossing back through the main thread.
pub type AudioLayerProducer = ringbuf::HeapProd<f32>;

/// Engine-internal sample format. Most macOS / Windows / Linux output
/// devices natively support 48 kHz stereo f32, so we pin those values
/// once and let every layer's resampler target them.
const ENGINE_RATE: u32 = 48_000;
const ENGINE_CHANNELS: u16 = 2;

/// Capacity of each per-layer ring buffer (interleaved samples).
/// 48 kHz × 2 ch × ~340 ms of headroom.
const LAYER_BUFFER_CAPACITY: usize = 32_768;

/// Per-layer atomic state read by the cpal callback.
///
/// `gain` is stored as the bit pattern of an `f32` so the UI can write
/// it from the main thread with `Ordering::Relaxed` and the audio
/// thread can read it lock-free.
pub struct AudioLayerControl {
    pub gain_bits: AtomicU32,
    /// Layer master fade (0.0..=1.0), folded into the audio path the
    /// same way it folds into the visual path. Drag the master to 0
    /// and both buses go silent / black at once.
    pub master_bits: AtomicU32,
    pub muted: AtomicBool,
    pub soloed: AtomicBool,
    /// Per-layer audio frames (channel-frames, not interleaved samples)
    /// the cpal callback has popped from this source's ring buffer.
    /// Used by `Layer::tick` as the master clock when audio is present:
    /// the video pump targets `(consumed - origin) / sample_rate` seconds
    /// of source time. Counter advances only while cpal is actually
    /// pulling samples — pausing freezes both audio and video together.
    /// Muted / solo-silenced layers still drain the ring (so the counter
    /// advances), which is what keeps the video moving while the audio
    /// is silenced.
    ///
    /// Note: `consumed_samples` counts frames *popped from the ring* —
    /// not frames *audible at the DAC*. The samples we pop in a given
    /// callback won't reach the speakers until ~`lead_nanos` later, so
    /// using this counter on its own makes the picture lead the sound
    /// by the playout lead time (typically 10–80 ms). The Layer pairs
    /// it with `lead_nanos` to correct for that static offset.
    pub consumed_samples: AtomicU64,
    /// Predicted playout lead time (`playback - callback`, from cpal's
    /// `OutputStreamTimestamp`) measured at the most recent callback,
    /// in nanoseconds. The samples just popped will be audible
    /// approximately this far in the future. The Layer subtracts
    /// `lead_nanos * sample_rate / 1e9` from `consumed_samples` to get
    /// the *audible* sample count — the actual PTS of what the user is
    /// hearing right now.
    ///
    /// Updated together with `consumed_samples` in the cpal callback;
    /// readers may see a stale pair (one updated, the other not) but
    /// the inconsistency is bounded by one callback's worth of samples
    /// and is dwarfed by the natural buffer jitter, so we tolerate it
    /// without seqlock-style ceremony.
    pub lead_nanos: AtomicU64,
}

impl Default for AudioLayerControl {
    fn default() -> Self {
        Self {
            gain_bits: AtomicU32::new(1.0_f32.to_bits()),
            master_bits: AtomicU32::new(1.0_f32.to_bits()),
            muted: AtomicBool::new(false),
            soloed: AtomicBool::new(false),
            consumed_samples: AtomicU64::new(0),
            lead_nanos: AtomicU64::new(0),
        }
    }
}

impl AudioLayerControl {
    pub fn set_gain(&self, gain: f32) {
        self.gain_bits.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed))
    }

    pub fn set_master(&self, master: f32) {
        self.master_bits.store(master.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn master(&self) -> f32 {
        f32::from_bits(self.master_bits.load(Ordering::Relaxed))
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn set_soloed(&self, soloed: bool) {
        self.soloed.store(soloed, Ordering::Relaxed);
    }

    pub fn is_soloed(&self) -> bool {
        self.soloed.load(Ordering::Relaxed)
    }

    /// Raw popped-from-ring counter. Use `audible_samples` for sync;
    /// this is exposed for telemetry / debugging only.
    #[allow(dead_code)]
    pub fn consumed_samples(&self) -> u64 {
        self.consumed_samples.load(Ordering::Relaxed)
    }

    /// Latest cpal-reported playout lead time, in nanoseconds. Exposed
    /// for telemetry / debugging; `audible_samples` already folds this
    /// in for the sync path.
    #[allow(dead_code)]
    pub fn lead_nanos(&self) -> u64 {
        self.lead_nanos.load(Ordering::Relaxed)
    }

    /// "Audible samples" — frames the DAC has actually played, derived
    /// from `consumed_samples` minus the predicted playout lead time.
    /// This is the value the video pump should target.
    ///
    /// Saturating at zero handles the first few callbacks where the
    /// ring buffer hasn't drained enough samples to cover the lead
    /// time yet (e.g. on first play, `consumed_samples` may be 2048
    /// but `lead_samples` may be 5760 — picture should sit at PTS 0,
    /// not jump backwards).
    pub fn audible_samples(&self, sample_rate: u32) -> u64 {
        let consumed = self.consumed_samples.load(Ordering::Relaxed);
        let lead = self.lead_nanos.load(Ordering::Relaxed);
        // lead_samples = lead_nanos * sample_rate / 1e9, computed with
        // u128 to keep the multiply from wrapping at unusual rates.
        let lead_samples =
            ((lead as u128).saturating_mul(sample_rate as u128) / 1_000_000_000u128) as u64;
        consumed.saturating_sub(lead_samples)
    }
}

/// Producer-side handle for one layer. Holds the cpal-callback's view
/// of the layer (the `control` Arc with gain/master/mute atomics) and
/// the SPSC producer end of the ring buffer.
///
/// The producer is `Option` because once a clip is loaded the layer
/// hands the producer to its decode worker thread (via `take_producer`)
/// and reclaims it when the worker exits (via `set_producer`). While
/// the worker owns it, the field is `None`.
pub struct AudioLayerHandle {
    pub control: Arc<AudioLayerControl>,
    pub producer: Option<AudioLayerProducer>,
}

impl AudioLayerHandle {
    /// Move the producer out so a decode worker can own it. Leaves the
    /// handle's producer slot empty until the worker returns it on exit.
    pub fn take_producer(&mut self) -> Option<AudioLayerProducer> {
        self.producer.take()
    }

    /// Park a producer back on the handle (called when a worker thread
    /// joins so the next clip load can hand it to a new worker).
    pub fn set_producer(&mut self, prod: AudioLayerProducer) {
        self.producer = Some(prod);
    }
}

/// Consumer-side state held by the cpal callback.
struct MixSource {
    control: Arc<AudioLayerControl>,
    consumer: ringbuf::HeapCons<f32>,
}

/// One audio output stream (cpal) that mixes every layer's ring buffer.
///
/// Construction is two-stage: `new(layer_count)` allocates the ring
/// buffers and returns the producer-side `AudioLayerHandle`s, while
/// stashing the consumer sides on the engine. Then `start(device)`
/// consumes those stashed consumers to build the cpal stream. This
/// keeps the (lock-free) wiring out of the AppState's hot path.
pub struct AudioEngine {
    host: cpal::Host,
    master: Arc<AtomicU32>,
    /// Aggregate "is any layer soloed" flag. Owned here (not on
    /// `Composition`) so it survives audio-device switches alongside
    /// the per-layer `AudioLayerControl` Arcs. The same Arc is cloned
    /// into the cpal mix closure and into `Composition` so both the
    /// audio thread and the render path can read it lock-free.
    any_solo_active: Arc<AtomicBool>,
    /// Consumer ends waiting to be wired into a cpal stream. `start`
    /// drains this; subsequent `start` calls (device switch) need to
    /// rebuild while preserving the same ring buffers — a V2 task.
    pending: Option<Vec<MixSource>>,
    stream: Option<cpal::Stream>,
    current_device: Option<String>,
}

impl AudioEngine {
    /// Build the engine + per-layer producer handles. The cpal stream
    /// isn't started yet — call `start(device_name)` afterwards.
    pub fn new(layer_count: usize) -> (Self, Vec<AudioLayerHandle>) {
        let host = cpal::default_host();
        let mut handles = Vec::with_capacity(layer_count);
        let mut sources = Vec::with_capacity(layer_count);

        for _ in 0..layer_count {
            let rb = HeapRb::<f32>::new(LAYER_BUFFER_CAPACITY);
            let (producer, consumer) = rb.split();
            let control = Arc::new(AudioLayerControl::default());
            sources.push(MixSource { control: control.clone(), consumer });
            handles.push(AudioLayerHandle { control, producer: Some(producer) });
        }

        let engine = Self {
            host,
            master: Arc::new(AtomicU32::new(1.0_f32.to_bits())),
            any_solo_active: Arc::new(AtomicBool::new(false)),
            pending: Some(sources),
            stream: None,
            current_device: None,
        };
        (engine, handles)
    }

    /// Hand out a clone of the shared "any layer soloed" flag.
    /// `Composition` keeps a clone so the render path can gate
    /// non-soloed layers in lockstep with the audio thread.
    pub fn any_solo_active(&self) -> Arc<AtomicBool> {
        self.any_solo_active.clone()
    }

    pub fn list_device_names(&self) -> Vec<String> {
        self.host
            .output_devices()
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|d| d.description().ok().map(|desc| desc.name().to_owned()))
            .collect()
    }

    pub fn default_device_name(&self) -> Option<String> {
        self.host
            .default_output_device()
            .and_then(|d| d.description().ok().map(|desc| desc.name().to_owned()))
    }

    pub fn current_device_name(&self) -> Option<&str> {
        self.current_device.as_deref()
    }

    pub fn audio_config(&self) -> AudioConfig {
        AudioConfig {
            sample_rate: ENGINE_RATE,
            channels: ENGINE_CHANNELS,
        }
    }

    pub fn master_volume(&self) -> f32 {
        f32::from_bits(self.master.load(Ordering::Relaxed))
    }

    pub fn set_master_volume(&self, v: f32) {
        let v = v.clamp(0.0, 4.0);
        self.master.store(v.to_bits(), Ordering::Relaxed);
    }

    /// Open the cpal stream on `device_name` (or the host default if
    /// `None`). Called once at boot from the initial `pending` consumer
    /// list set up in `new`.
    pub fn start(&mut self, device_name: Option<&str>) -> Result<()> {
        let sources = self
            .pending
            .take()
            .ok_or_else(|| anyhow!("audio engine already started"))?;
        self.build_stream(device_name, sources)
    }

    /// Switch to a different output device at runtime. Drops the active
    /// stream, allocates fresh per-layer ring buffers (preserving each
    /// layer's existing `AudioLayerControl` so gain / mute settings
    /// survive the swap), rewires every `Layer`'s producer handle, and
    /// builds a new stream around the new consumer ends. The user
    /// experiences a tiny audio gap (one cpal teardown + setup); ring
    /// buffers are emptied along with the dropped stream so we don't
    /// emit a pile of stale samples on the new device.
    pub fn switch_device(
        &mut self,
        layers: &mut [crate::layer::Layer],
        device_name: Option<&str>,
    ) -> Result<()> {
        // Drop the existing stream first; this ends the old callback
        // and frees its consumer ends, so the producers we're about
        // to overwrite become irrelevant.
        self.stream = None;

        let mut sources = Vec::with_capacity(layers.len());
        for layer in layers.iter_mut() {
            let rb = HeapRb::<f32>::new(LAYER_BUFFER_CAPACITY);
            let (producer, consumer) = rb.split();
            // LOAD-BEARING: reuse the layer's existing `AudioLayerControl`
            // Arc so the `consumed_samples` / `lead_nanos` counters that
            // anchor A/V sync survive the device swap. Allocating a
            // fresh control here would reset both counters to zero,
            // making `Layer::audio_target_pts` race forward by the
            // pre-swap consumed count and producing a one-off picture
            // jump every time the user switches devices.
            let control = layer
                .audio_control()
                .unwrap_or_else(|| Arc::new(AudioLayerControl::default()));
            sources.push(MixSource {
                control: control.clone(),
                consumer,
            });
            layer.replace_audio_handle(AudioLayerHandle { control, producer: Some(producer) });
        }

        self.build_stream(device_name, sources)
    }

    fn build_stream(
        &mut self,
        device_name: Option<&str>,
        sources: Vec<MixSource>,
    ) -> Result<()> {
        let device = pick_output_device(&self.host, device_name)?;
        let device_label = device.description().ok().map(|d| d.name().to_owned());

        let config = pick_supported_config(&device)?;
        let stream_config: cpal::StreamConfig = config.config();
        let sample_format = config.sample_format();

        info!(
            "audio engine starting on {:?}, {} ch {} Hz, format {:?}",
            device_label, stream_config.channels, stream_config.sample_rate, sample_format,
        );

        if stream_config.sample_rate != ENGINE_RATE || stream_config.channels != ENGINE_CHANNELS {
            warn!(
                "device config {} ch @ {} Hz differs from engine {} ch @ {} Hz; \
                 the OS may resample",
                stream_config.channels, stream_config.sample_rate, ENGINE_CHANNELS, ENGINE_RATE,
            );
        }

        let mut mix_sources = sources;
        let master = self.master.clone();
        let any_solo = self.any_solo_active.clone();
        let mut scratch = vec![0.0_f32; 8192];

        let err_handler = |e| warn!("audio stream error: {e}");

        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &stream_config,
                move |output: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    // cpal hands us a predicted-playback / callback pair
                    // per buffer. The delta is the "lead time" — how far
                    // in the future the samples we're about to pop will
                    // be heard. We propagate it into each layer's
                    // `lead_nanos` atomic so `Layer::tick` can subtract
                    // it from `consumed_samples` and drive the video
                    // pump off audible (not popped) audio time.
                    let ts = info.timestamp();
                    let lead = ts
                        .playback
                        .duration_since(&ts.callback)
                        .unwrap_or(Duration::ZERO);
                    mix(output, &mut mix_sources, &master, &any_solo, &mut scratch, lead);
                },
                err_handler,
                None,
            ),
            other => {
                return Err(anyhow!(
                    "audio device sample format {other:?} not supported (V1 wants F32)"
                ));
            }
        }
        .with_context(|| format!("building output stream on {device_label:?}"))?;

        stream.play().context("starting audio stream")?;

        self.stream = Some(stream);
        self.current_device = device_label;
        Ok(())
    }
}

fn pick_output_device(host: &cpal::Host, name: Option<&str>) -> Result<cpal::Device> {
    if let Some(target) = name {
        for d in host.output_devices().context("listing output devices")? {
            if d.description().ok().map(|desc| desc.name().to_owned()).as_deref() == Some(target) {
                return Ok(d);
            }
        }
        warn!("audio device {target:?} not found; falling back to default");
    }
    host.default_output_device()
        .ok_or_else(|| anyhow!("no default audio output device"))
}

fn pick_supported_config(device: &cpal::Device) -> Result<cpal::SupportedStreamConfig> {
    if let Ok(configs) = device.supported_output_configs() {
        for cfg in configs {
            if cfg.sample_format() == cpal::SampleFormat::F32
                && cfg.channels() == ENGINE_CHANNELS
                && cfg.min_sample_rate() <= ENGINE_RATE
                && cfg.max_sample_rate() >= ENGINE_RATE
            {
                return Ok(cfg.with_sample_rate(ENGINE_RATE));
            }
        }
    }
    device
        .default_output_config()
        .context("device has no default output config")
}

fn mix(
    output: &mut [f32],
    sources: &mut [MixSource],
    master: &AtomicU32,
    any_solo_active: &AtomicBool,
    scratch: &mut [f32],
    lead: Duration,
) {
    output.fill(0.0);
    let any_solo = any_solo_active.load(Ordering::Relaxed);
    let channels = ENGINE_CHANNELS as usize;
    // Capped at u64 to avoid platform-specific overflow on 32-bit
    // `lead.as_nanos()` returns (it's `u128`); a sensible playout
    // buffer is < 1 s, so this cap never trims real data.
    let lead_nanos: u64 = lead.as_nanos().min(u64::MAX as u128) as u64;
    for src in sources.iter_mut() {
        // Publish the lead time *before* we pop, so a reader that
        // catches the inconsistency window sees a slightly-old
        // consumed_samples with the *current* lead — biases the
        // computed audible time backwards by at most one buffer, which
        // is the right side to err on (no future-PTS uploads).
        src.control.lead_nanos.store(lead_nanos, Ordering::Relaxed);
        // Mute is the hard kill; solo silences every non-soloed source
        // when at least one source is soloed. Mute wins over solo. In
        // either case we still drain the ring + advance the consumed
        // counter so the video pump keeps rolling — that way un-soloing
        // (or un-muting) resumes cleanly instead of jumping forward.
        //
        // Two Relaxed loads (`is_muted`, then `is_soloed`) can tear if
        // the user toggles one between them — bounded to one callback
        // (~10 ms) of slightly wrong gating, which is below perception.
        // No stronger ordering would close the gap with the prior
        // `any_solo` load anyway, so don't reach for SeqCst.
        let silenced = src.control.is_muted() || (any_solo && !src.control.is_soloed());
        if silenced {
            // Take cap matches the *active* branch below
            // (`output.len().min(scratch.len())`) — load-bearing: if
            // silenced sources drained at scratch length while active
            // sources drained at output length, mute/solo toggles
            // would race the video pump's clock forward.
            let take = output.len().min(scratch.len());
            let popped = src.consumer.pop_slice(&mut scratch[..take]);
            advance_consumed(&src.control, popped, channels);
            continue;
        }
        let effective_gain = src.control.gain() * src.control.master();
        let take = output.len().min(scratch.len());
        let popped = src.consumer.pop_slice(&mut scratch[..take]);
        advance_consumed(&src.control, popped, channels);
        if effective_gain == 0.0 {
            continue;
        }
        for i in 0..popped {
            output[i] += scratch[i] * effective_gain;
        }
    }
    let master_g = f32::from_bits(master.load(Ordering::Relaxed));
    for s in output.iter_mut() {
        *s = (*s * master_g).clamp(-1.0, 1.0);
    }
}

fn advance_consumed(control: &AudioLayerControl, popped_interleaved: usize, channels: usize) {
    if popped_interleaved == 0 || channels == 0 {
        return;
    }
    let frames = (popped_interleaved / channels) as u64;
    if frames > 0 {
        control.consumed_samples.fetch_add(frames, Ordering::Relaxed);
    }
}
