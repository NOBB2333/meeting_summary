#![cfg(not(target_family = "wasm"))]

pub use gpui_wgpu::CompositorGpuHint;

#[path = "../src/adapter_selection.rs"]
mod adapter_selection;

use wgpu::{AdapterInfo, Backend, DeviceType};

fn info(device_type: DeviceType, backend: Backend) -> AdapterInfo {
    AdapterInfo {
        name: "synthetic adapter".into(),
        vendor: 0x8086,
        device: 1,
        device_type,
        device_pci_bus_id: String::new(),
        driver: String::new(),
        driver_info: String::new(),
        backend,
        subgroup_min_size: 4,
        subgroup_max_size: 32,
        transient_saves_memory: false,
    }
}

#[test]
fn software_presentation_prefers_opengl() {
    let gl = info(DeviceType::Cpu, Backend::Gl);
    let vulkan = info(DeviceType::Cpu, Backend::Vulkan);
    assert!(
        adapter_selection::priority(&gl, None, None, true)
            < adapter_selection::priority(&vulkan, None, None, true)
    );
}

#[test]
fn hardware_still_precedes_software_opengl() {
    let hardware = info(DeviceType::IntegratedGpu, Backend::Vulkan);
    let software = info(DeviceType::Cpu, Backend::Gl);
    assert!(
        adapter_selection::priority(&hardware, None, None, true)
            < adapter_selection::priority(&software, None, None, true)
    );
}

#[test]
fn hardware_backend_preference_is_preserved() {
    let gl = info(DeviceType::IntegratedGpu, Backend::Gl);
    let vulkan = info(DeviceType::IntegratedGpu, Backend::Vulkan);
    assert!(
        adapter_selection::priority(&vulkan, None, None, true)
            < adapter_selection::priority(&gl, None, None, true)
    );
}

#[test]
fn headless_software_vulkan_preference_is_preserved() {
    let gl = info(DeviceType::Cpu, Backend::Gl);
    let vulkan = info(DeviceType::Cpu, Backend::Vulkan);
    assert!(
        adapter_selection::priority(&vulkan, None, None, false)
            < adapter_selection::priority(&gl, None, None, false)
    );
}
