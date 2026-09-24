//! Desktop device facts and device-registration callbacks (port of
//! src/device_info.cpp). Owner: the SDK bootstrap / device info / progress port.

use crate::sys;

/// Installs the desktop device-registration callbacks on the commons device
/// manager. Must run before SDK phase 2 so registration carries real hardware
/// info instead of being skipped for missing callbacks.
pub fn install_device_callbacks() -> sys::rac_result_t {
    todo!("bootstrap port: install_device_callbacks")
}

/// Local CPU/OS/architecture facts, computed the same way the device
/// registration payload is, without touching the device manager. Used by
/// `wally about`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceSnapshot {
    /// CPU model / chip name
    pub chip: String,
    /// e.g. "macOS 15.1"
    pub os_version: String,
    /// e.g. "arm64"
    pub architecture: String,
    pub core_count: i32,
    pub performance_cores: i32,
    pub efficiency_cores: i32,
}

pub fn collect_device_snapshot() -> DeviceSnapshot {
    todo!("bootstrap port: collect_device_snapshot")
}
