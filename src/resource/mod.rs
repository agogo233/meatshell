#[path = "impls/system.rs"]
pub(crate) mod system;
#[path = "struct/system.rs"]
mod system_types;

pub(crate) use system_types::{
    LocalGpuInfo, LocalHardwareInfo, LocalSnap, NetHist, SystemSampler, SystemSnapshot, TabStatus,
    TabStatuses,
};
