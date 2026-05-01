use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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
}

impl Default for AudioLayerControl {
    fn default() -> Self {
        Self {
            gain_bits: AtomicU32::new(1.0_f32.to_bits()),
            master_bits: AtomicU32::new(1.0_f32.to_bits()),
            muted: AtomicBool::new(false),
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
            pending: Some(sources),
            stream: None,
            current_device: None,
        };
        (engine, handles)
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
        let mut scratch = vec![0.0_f32; 8192];

        let err_handler = |e| warn!("audio stream error: {e}");

        let stream = match sample_format {
            cpal::SampleFormat::F32 => device.build_output_stream(
                &stream_config,
                move |output: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                    mix(output, &mut mix_sources, &master, &mut scratch);
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
    scratch: &mut [f32],
) {
    output.fill(0.0);
    for src in sources.iter_mut() {
        if src.control.is_muted() {
            // Drain so the buffer doesn't backlog while muted.
            let _ = src.consumer.pop_slice(scratch);
            continue;
        }
        let effective_gain = src.control.gain() * src.control.master();
        if effective_gain == 0.0 {
            let _ = src.consumer.pop_slice(scratch);
            continue;
        }
        let take = output.len().min(scratch.len());
        let popped = src.consumer.pop_slice(&mut scratch[..take]);
        for i in 0..popped {
            output[i] += scratch[i] * effective_gain;
        }
    }
    let master_g = f32::from_bits(master.load(Ordering::Relaxed));
    for s in output.iter_mut() {
        *s = (*s * master_g).clamp(-1.0, 1.0);
    }
}
