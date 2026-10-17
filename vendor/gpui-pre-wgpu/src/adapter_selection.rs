//! Desktop adapter ranking shared with the focused regression test.
use crate::CompositorGpuHint;

pub(crate) fn priority(
    info: &wgpu::AdapterInfo,
    device_id_filter: Option<u32>,
    compositor_gpu: Option<&CompositorGpuHint>,
    presenting: bool,
) -> (u8, u8, u8, u8) {
    // Backends like OpenGL report device=0 for all adapters, so
    // device-based matching is only meaningful when non-zero.
    let device_known = info.device != 0;
    let user_override: u8 = match device_id_filter {
        Some(id) if device_known && info.device == id => 0,
        _ => 1,
    };
    let compositor_match: u8 = match compositor_gpu {
        Some(hint)
            if device_known && info.vendor == hint.vendor_id && info.device == hint.device_id =>
        {
            0
        }
        _ => 1,
    };
    let type_priority: u8 = match info.device_type {
        wgpu::DeviceType::DiscreteGpu => 0,
        wgpu::DeviceType::IntegratedGpu => 1,
        wgpu::DeviceType::Other => 2,
        wgpu::DeviceType::VirtualGpu => 3,
        wgpu::DeviceType::Cpu => 4,
    };
    let backend_priority: u8 = match (presenting, info.device_type, info.backend) {
        (true, wgpu::DeviceType::Cpu, wgpu::Backend::Gl) => 0,
        (true, wgpu::DeviceType::Cpu, _) => 1,
        (_, _, wgpu::Backend::Vulkan | wgpu::Backend::Metal | wgpu::Backend::Dx12) => 0,
        _ => 1,
    };

    (
        user_override,
        compositor_match,
        type_priority,
        backend_priority,
    )
}
