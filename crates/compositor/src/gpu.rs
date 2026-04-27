use avengine_core::AvError;

/// Process-wide GPU state: instance, adapter, device, and queue.
///
/// Created once and shared across every `WindowSurface`. Keeping the
/// `Instance` here is what lets later surfaces (a second window for
/// fullscreen output, for example) sit on the same adapter and avoids
/// re-uploading the video texture per window.
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl GpuContext {
    /// Create the instance and (if a hint surface is available) request an
    /// adapter that's compatible with it. The `hint_surface` is *only* used
    /// for adapter selection; subsequent surfaces created from `instance`
    /// will reuse the same adapter automatically.
    pub async fn new(hint_surface: Option<&wgpu::Surface<'static>>) -> Result<Self, AvError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        Self::with_instance(instance, hint_surface).await
    }

    pub async fn with_instance(
        instance: wgpu::Instance,
        hint_surface: Option<&wgpu::Surface<'static>>,
    ) -> Result<Self, AvError> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: hint_surface,
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| AvError::gpu("no suitable GPU adapter"))?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("avengine.device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults()
                        .using_resolution(adapter.limits()),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .map_err(AvError::gpu)?;

        Ok(Self { instance, adapter, device, queue })
    }
}
