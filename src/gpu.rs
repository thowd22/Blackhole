//! Pick the GPU for DirectML. ORT's DirectML `device_id` is the DXGI adapter
//! index; DirectML's own default can land on an integrated GPU that happens to
//! enumerate first, so choose the hardware adapter with the most dedicated
//! VRAM ourselves.

use crate::util::from_wide;
use std::sync::OnceLock;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};

#[derive(Clone, Debug)]
pub struct Adapter {
    pub index: i32,
    pub name: String,
    pub vram_mb: u64,
}

fn enumerate() -> Vec<Adapter> {
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return out };
        for i in 0.. {
            let Ok(adapter) = factory.EnumAdapters1(i) else { break };
            let Ok(desc) = adapter.GetDesc1() else { continue };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            out.push(Adapter { index: i as i32, name: from_wide(&desc.Description), vram_mb: (desc.DedicatedVideoMemory / 1_048_576) as u64 });
        }
    }
    out
}

/// The adapter DirectML should use, or None if there is no usable GPU.
/// `BLACKHOLE_DML_DEVICE=<index>` overrides for testing.
pub fn preferred() -> Option<Adapter> {
    static PICK: OnceLock<Option<Adapter>> = OnceLock::new();
    PICK.get_or_init(|| {
        let adapters = enumerate();
        if let Some(forced) = std::env::var("BLACKHOLE_DML_DEVICE").ok().and_then(|v| v.parse::<i32>().ok()) {
            return adapters.into_iter().find(|a| a.index == forced);
        }
        // Integrated GPUs report little or no dedicated memory; anything under
        // 1 GB is not worth handing an LLM to.
        adapters.into_iter().filter(|a| a.vram_mb >= 1024).max_by_key(|a| a.vram_mb)
    })
    .clone()
}
