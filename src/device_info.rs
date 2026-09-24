//! Desktop device facts and device-registration callbacks (port of
//! src/device_info.cpp). Owner: the SDK bootstrap / device info / progress port.

use std::ffi::CString;
use std::sync::{Mutex, OnceLock};

use crate::sys;

/// Installs the desktop device-registration callbacks on the commons device
/// manager. Must run before SDK phase 2 so registration carries real hardware
/// info instead of being skipped for missing callbacks.
pub fn install_device_callbacks() -> sys::rac_result_t {
    {
        let mut info = state().lock().unwrap_or_else(|e| e.into_inner());
        let mut device_id =
            [0 as std::os::raw::c_char; sys::RAC_DEVICE_ID_BUFFER_MIN_SIZE as usize];
        // SAFETY: device_id is a stack buffer of exactly the size the ABI
        // requires (RAC_DEVICE_ID_BUFFER_MIN_SIZE); the SDK only ever writes a
        // NUL-terminated string within it.
        let rc = unsafe {
            sys::rac_device_get_or_create_persistent_id(device_id.as_mut_ptr(), device_id.len())
        };
        if rc == sys::SUCCESS {
            // SAFETY: rac_device_get_or_create_persistent_id NUL-terminates on
            // success.
            let text = unsafe { std::ffi::CStr::from_ptr(device_id.as_ptr()) };
            if !text.to_bytes().is_empty() {
                info.device_id = CString::new(text.to_bytes()).unwrap_or_default();
            }
        }
    }

    let callbacks = sys::rac_device_callbacks_t {
        get_device_info: Some(device_get_info),
        get_device_id: Some(device_get_id),
        is_registered: Some(device_is_registered),
        set_registered: Some(device_set_registered),
        http_post: Some(device_http_post),
        user_data: std::ptr::null_mut(),
    };
    // SAFETY: `callbacks` is a plain-data struct of function pointers + a null
    // user_data; the SDK copies it internally (per the header's own doc
    // comment), so it does not need to outlive this call.
    unsafe { sys::rac_device_manager_set_callbacks(&callbacks) }
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
    let mut info = DeviceInfoState::default();
    collect_device_info(&mut info);
    DeviceSnapshot {
        chip: cstr_to_string(&info.chip),
        os_version: cstr_to_string(&info.os_version),
        architecture: cstr_to_string(&info.architecture),
        core_count: info.core_count,
        performance_cores: info.performance_cores,
        efficiency_cores: info.efficiency_cores,
    }
}

// -----------------------------------------------------------------------------
// Internal state + platform data collection
// -----------------------------------------------------------------------------

/// Mirrors C++'s function-local static `DeviceInfoState`: every field handed
/// out through `rac_device_registration_info_t` as a raw pointer is kept as a
/// `CString` here (Rust's `String` is not NUL-terminated, unlike
/// `std::string::c_str()`) so the pointer stays valid for the duration of the
/// synchronous `device_get_info`/`device_get_id`/`device_http_post` calls the
/// SDK makes into it.
#[derive(Default)]
struct DeviceInfoState {
    device_id: CString,
    model: CString,
    name: CString,
    platform: CString,
    os_version: CString,
    form_factor: CString,
    architecture: CString,
    chip: CString,
    gpu_family: CString,
    battery_state: CString,
    fingerprint: CString,
    battery_level: f64,
    total_memory: i64,
    available_memory: i64,
    core_count: i32,
    performance_cores: i32,
    efficiency_cores: i32,
    registered: bool,
    http_body: CString,
    http_error: CString,
}

fn state() -> &'static Mutex<DeviceInfoState> {
    static STATE: OnceLock<Mutex<DeviceInfoState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(DeviceInfoState::default()))
}

fn cstr_to_string(value: &CString) -> String {
    value.to_string_lossy().into_owned()
}

/// A `CString` from an owned `String`, replacing interior NULs with nothing
/// (device facts never legitimately contain one; `CString::new` failing on
/// attacker-controlled input is not a concern here, but the code must not
/// panic/unwrap on it regardless).
fn cstring_from(value: impl AsRef<str>) -> CString {
    CString::new(value.as_ref()).unwrap_or_default()
}

fn trim(value: &str) -> &str {
    value.trim_matches(|c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n')
}

/// Port of `std::strtoll(s, nullptr, 10)`: skips leading ASCII whitespace, an
/// optional sign, then the longest run of decimal digits, ignoring any
/// trailing text (e.g. a unit suffix like " kB"). Returns 0 when no digits
/// are found, matching the C locale `isspace` + `strtol` family behaviour
/// C++ relies on in `meminfo_bytes`. Out-of-range values clamp to
/// `i64::MAX`/`i64::MIN`, mirroring strtoll's `ERANGE` clamping.
///
/// Only meminfo_bytes() (Linux-only) calls this outside of tests, so it is
/// gated the same way to avoid a dead-code warning on other platforms.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn parse_leading_i64(s: &str) -> i64 {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let negative = match bytes.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digits_start {
        return 0;
    }
    // s is valid UTF-8 and [digits_start, i) is an ASCII digit run, so this
    // slice is always valid UTF-8.
    let digits = &s[digits_start..i];
    match digits.parse::<i64>() {
        Ok(v) => {
            if negative {
                -v
            } else {
                v
            }
        }
        Err(_) => {
            if negative {
                i64::MIN
            } else {
                i64::MAX
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Linux
// -----------------------------------------------------------------------------

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use super::{trim, DeviceInfoState};
    use std::io::BufRead;

    fn read_first_line(path: &str) -> String {
        let Ok(file) = std::fs::File::open(path) else {
            return String::new();
        };
        let mut line = String::new();
        if std::io::BufReader::new(file)
            .read_line(&mut line)
            .unwrap_or(0)
            > 0
        {
            trim(line.trim_end_matches('\n')).to_string()
        } else {
            String::new()
        }
    }

    fn os_release_pretty_name() -> String {
        let Ok(file) = std::fs::File::open("/etc/os-release") else {
            return String::new();
        };
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
                let mut value = trim(rest).to_string();
                if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
                    value = value[1..value.len() - 1].to_string();
                }
                return value;
            }
        }
        String::new()
    }

    fn cpuinfo_model_name() -> String {
        let Ok(file) = std::fs::File::open("/proc/cpuinfo") else {
            return String::new();
        };
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            if line.starts_with("model name") || line.starts_with("Hardware") {
                if let Some(colon) = line.find(':') {
                    return trim(&line[colon + 1..]).to_string();
                }
            }
        }
        String::new()
    }

    fn meminfo_bytes(key: &str) -> i64 {
        let Ok(file) = std::fs::File::open("/proc/meminfo") else {
            return 0;
        };
        let prefix = format!("{key}:");
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            if let Some(rest) = line.strip_prefix(&prefix) {
                // C++ uses strtoll(line.c_str() + prefix.size(), nullptr, 10),
                // which parses the leading digit run and ignores the trailing
                // " kB" unit suffix; a plain parse::<i64>() on the trimmed
                // remainder would reject "16130004 kB" outright.
                let kib = super::parse_leading_i64(rest);
                return if kib > 0 { kib * 1024 } else { 0 };
            }
        }
        0
    }

    fn has_battery_dir() -> Option<String> {
        let entries = std::fs::read_dir("/sys/class/power_supply").ok()?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(rest) = name.strip_prefix("BAT") {
                let _ = rest;
                return Some(entry.path().to_string_lossy().into_owned());
            }
        }
        None
    }

    fn linux_gpu_family() -> String {
        if std::path::Path::new("/proc/driver/nvidia/version").exists() {
            return "nvidia".to_string();
        }
        let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
            return "unknown".to_string();
        };
        for entry in entries.flatten() {
            let card = entry.file_name();
            let card = card.to_string_lossy();
            if !card.starts_with("card") {
                continue;
            }
            let uevent_path = entry.path().join("device/uevent");
            let Ok(file) = std::fs::File::open(&uevent_path) else {
                continue;
            };
            for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
                let Some(driver) = line.strip_prefix("DRIVER=") else {
                    continue;
                };
                let driver = trim(driver);
                match driver {
                    "amdgpu" | "radeon" => return "amd".to_string(),
                    "i915" | "xe" => return "intel".to_string(),
                    "nvidia" | "nouveau" => return "nvidia".to_string(),
                    _ => {}
                }
            }
        }
        "unknown".to_string()
    }

    fn linux_core_topology(core_count: i32) -> (i32, i32) {
        let mut max_freqs = Vec::with_capacity(core_count.max(0) as usize);
        let mut highest: i64 = 0;
        for cpu in 0..core_count {
            let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq");
            let value = read_first_line(&path);
            let freq: i64 = if value.is_empty() {
                0
            } else {
                value.parse().unwrap_or(0)
            };
            if freq <= 0 {
                return (core_count, 0);
            }
            max_freqs.push(freq);
            highest = highest.max(freq);
        }
        let performance = max_freqs.iter().filter(|&&f| f == highest).count() as i32;
        if performance == 0 || performance == core_count {
            return (core_count, 0);
        }
        (performance, core_count - performance)
    }

    pub(super) fn collect_device_info(info: &mut DeviceInfoState) {
        info.platform = super::cstring_from("linux");

        let mut model = read_first_line("/sys/devices/virtual/dmi/id/product_name");
        if model.is_empty() {
            model = "Linux Desktop".to_string();
        }
        info.model = super::cstring_from(&model);

        let mut name = read_first_line("/etc/hostname");
        if name.is_empty() {
            let mut buf = [0u8; 256];
            // SAFETY: `buf` is a valid, correctly-sized stack buffer; libc
            // writes at most `buf.len() - 1` bytes plus a NUL terminator.
            let rc =
                unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len() - 1) };
            if rc == 0 {
                // SAFETY: gethostname NUL-terminates on success within `buf`.
                let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr() as *const libc::c_char) };
                name = cstr.to_string_lossy().into_owned();
            }
        }
        if name.is_empty() {
            name = model.clone();
        }
        info.name = super::cstring_from(&name);

        let mut os_version = os_release_pretty_name();
        if os_version.is_empty() {
            os_version = "Linux".to_string();
        }
        info.os_version = super::cstring_from(&os_version);

        let mut chip = cpuinfo_model_name();
        if chip.is_empty() {
            chip = "unknown".to_string();
        }
        info.chip = super::cstring_from(&chip);

        info.total_memory = meminfo_bytes("MemTotal");
        info.available_memory = meminfo_bytes("MemAvailable");

        // SAFETY: sysconf with a well-known name constant is always safe.
        let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        info.core_count = if online > 0 { online as i32 } else { 1 };
        let (perf, eff) = linux_core_topology(info.core_count);
        info.performance_cores = perf;
        info.efficiency_cores = eff;

        // SAFETY: `uts` is a zeroed, correctly-sized stack struct; uname only
        // writes into it.
        let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
        // SAFETY: `&mut uts` is a valid pointer to a `libc::utsname`.
        let architecture = if unsafe { libc::uname(&mut uts) } == 0 {
            // SAFETY: uname NUL-terminates `machine` on success.
            let cstr = unsafe { std::ffi::CStr::from_ptr(uts.machine.as_ptr()) };
            let text = cstr.to_string_lossy().into_owned();
            if text.is_empty() {
                "unknown".to_string()
            } else {
                text
            }
        } else {
            "unknown".to_string()
        };
        info.architecture = super::cstring_from(&architecture);

        if let Some(battery_path) = has_battery_dir() {
            info.form_factor = super::cstring_from("laptop");
            let capacity = read_first_line(&format!("{battery_path}/capacity"));
            if !capacity.is_empty() {
                if let Ok(percent) = capacity.parse::<i64>() {
                    if (0..=100).contains(&percent) {
                        info.battery_level = percent as f64 / 100.0;
                    }
                }
            }
            let status = read_first_line(&format!("{battery_path}/status"));
            if info.battery_level >= 0.0 && !status.is_empty() {
                let state = match status.as_str() {
                    "Full" => "full",
                    "Charging" => "charging",
                    _ => "unplugged",
                };
                info.battery_state = super::cstring_from(state);
            }
        } else {
            info.form_factor = super::cstring_from("desktop");
        }

        info.gpu_family = super::cstring_from(linux_gpu_family());
    }
}

// -----------------------------------------------------------------------------
// macOS
// -----------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod platform {
    use super::{trim, DeviceInfoState};
    use std::ffi::{c_void, CStr, CString};
    use std::os::raw::{c_char, c_int, c_uchar};

    type CfIndex = isize;
    type CfTypeRef = *const c_void;
    type CfStringRef = *const c_void;
    type CfArrayRef = *const c_void;
    type CfDictionaryRef = *const c_void;
    type CfNumberRef = *const c_void;
    type CfBooleanRef = *const c_void;
    type CfAllocatorRef = *const c_void;
    type CfStringEncoding = u32;
    type CfNumberType = c_int;
    type CfStringCompareFlags = isize;
    type CfComparisonResult = isize;
    type Boolean = c_uchar;

    const K_CF_STRING_ENCODING_UTF8: CfStringEncoding = 0x0800_0100;
    const K_CF_NUMBER_DOUBLE_TYPE: CfNumberType = 13;
    const K_CF_COMPARE_EQUAL_TO: CfComparisonResult = 0;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFAllocatorDefault: CfAllocatorRef;
        fn CFRelease(cf: CfTypeRef);
        fn CFStringCreateWithCString(
            alloc: CfAllocatorRef,
            c_str: *const c_char,
            encoding: CfStringEncoding,
        ) -> CfStringRef;
        fn CFStringCompare(
            the_string1: CfStringRef,
            the_string2: CfStringRef,
            compare_options: CfStringCompareFlags,
        ) -> CfComparisonResult;
        fn CFArrayGetCount(the_array: CfArrayRef) -> CfIndex;
        fn CFArrayGetValueAtIndex(the_array: CfArrayRef, idx: CfIndex) -> *const c_void;
        fn CFDictionaryGetValue(the_dict: CfDictionaryRef, key: *const c_void) -> *const c_void;
        fn CFNumberGetValue(
            number: CfNumberRef,
            the_type: CfNumberType,
            value_ptr: *mut c_void,
        ) -> Boolean;
        fn CFBooleanGetValue(boolean: CfBooleanRef) -> Boolean;
    }

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> CfTypeRef;
        fn IOPSCopyPowerSourcesList(blob: CfTypeRef) -> CfArrayRef;
        fn IOPSGetPowerSourceDescription(blob: CfTypeRef, ps: CfTypeRef) -> CfDictionaryRef;
    }

    /// Builds a `CFStringRef` for a static ASCII key/value (IOPSKeys.h's
    /// `CFSTR(...)` macros). Leaked intentionally: these are process-lifetime
    /// constants looked up a handful of times per `wally` invocation.
    fn cfstr(value: &str) -> CfStringRef {
        let c_value = CString::new(value).unwrap_or_default();
        // SAFETY: `c_value` is a valid NUL-terminated C string for the
        // duration of this call; CoreFoundation copies its contents.
        unsafe {
            CFStringCreateWithCString(
                kCFAllocatorDefault,
                c_value.as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            )
        }
    }

    fn sysctl_string(key: &str) -> String {
        let c_key = CString::new(key).unwrap_or_default();
        let mut size: usize = 0;
        // SAFETY: querying the required buffer size with a NULL output
        // pointer is the documented two-call sysctlbyname pattern.
        let rc = unsafe {
            libc::sysctlbyname(
                c_key.as_ptr(),
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || size == 0 {
            return String::new();
        }
        let mut buf = vec![0u8; size];
        // SAFETY: `buf` has exactly `size` bytes of capacity, matching the
        // size sysctlbyname just reported.
        let rc = unsafe {
            libc::sysctlbyname(
                c_key.as_ptr(),
                buf.as_mut_ptr() as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return String::new();
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        trim(&String::from_utf8_lossy(&buf[..end])).to_string()
    }

    fn sysctl_i64(key: &str) -> i64 {
        let c_key = CString::new(key).unwrap_or_default();
        let mut value: i64 = 0;
        let mut size = std::mem::size_of::<i64>();
        // SAFETY: `value` is a correctly-sized i64 out-param; sysctlbyname
        // writes at most `size` bytes into it.
        let rc = unsafe {
            libc::sysctlbyname(
                c_key.as_ptr(),
                &mut value as *mut i64 as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            0
        } else {
            value
        }
    }

    fn macos_available_memory_bytes() -> i64 {
        // SAFETY: mach_host_self() takes no arguments and always succeeds.
        // `libc::mach_host_self` is deprecated in favor of the `mach2` crate,
        // which is not a dependency here; the underlying libc symbol is
        // stable and unaffected by that Rust-crate-level advice.
        #[allow(deprecated)]
        let host = unsafe { libc::mach_host_self() };
        let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count: libc::mach_msg_type_number_t = (std::mem::size_of::<libc::vm_statistics64>()
            / std::mem::size_of::<libc::integer_t>())
            as libc::mach_msg_type_number_t;
        // SAFETY: `stats`/`count` are correctly sized for HOST_VM_INFO64;
        // host_statistics64 only writes within them.
        let rc = unsafe {
            libc::host_statistics64(
                host,
                libc::HOST_VM_INFO64,
                &mut stats as *mut libc::vm_statistics64 as libc::host_info64_t,
                &mut count,
            )
        };
        if rc != libc::KERN_SUCCESS {
            return 0;
        }
        let page_size = sysctl_i64("hw.pagesize");
        if page_size <= 0 {
            return 0;
        }
        let free_pages = stats.free_count as i64 + stats.purgeable_count as i64;
        free_pages * page_size
    }

    fn macos_sample_battery(info: &mut DeviceInfoState) {
        // SAFETY: IOPSCopyPowerSourcesInfo returns an owned (+1 retained)
        // CFTypeRef or NULL; released below via CFRelease before returning.
        let blob = unsafe { IOPSCopyPowerSourcesInfo() };
        if blob.is_null() {
            return;
        }
        // SAFETY: `blob` is non-null and was just returned by
        // IOPSCopyPowerSourcesInfo above.
        let list = unsafe { IOPSCopyPowerSourcesList(blob) };
        if list.is_null() {
            // SAFETY: `blob` is a valid, owned CFTypeRef.
            unsafe { CFRelease(blob) };
            return;
        }

        // SAFETY: `list` is a valid CFArrayRef just returned above.
        let count = unsafe { CFArrayGetCount(list) };
        for i in 0..count {
            // SAFETY: `i` is within `[0, count)`, and `list` is valid for the
            // duration of this loop.
            let ps = unsafe { CFArrayGetValueAtIndex(list, i) };
            if ps.is_null() {
                continue;
            }
            // SAFETY: `blob`/`ps` are valid for the duration of this call.
            let desc = unsafe { IOPSGetPowerSourceDescription(blob, ps) };
            if desc.is_null() {
                continue;
            }

            let number_for = |key: &str| -> f64 {
                let key_ref = cfstr(key);
                // SAFETY: `desc` and `key_ref` are valid CF objects for the
                // duration of this call.
                let num = unsafe { CFDictionaryGetValue(desc, key_ref) };
                // SAFETY: `key_ref` is an owned CFStringRef from `cfstr`.
                unsafe { CFRelease(key_ref) };
                if num.is_null() {
                    return -1.0;
                }
                let mut value: f64 = -1.0;
                // SAFETY: `num` is a live CFNumberRef from the dictionary;
                // `value` is a correctly-sized f64 out-param.
                let ok = unsafe {
                    CFNumberGetValue(
                        num,
                        K_CF_NUMBER_DOUBLE_TYPE,
                        &mut value as *mut f64 as *mut c_void,
                    )
                };
                if ok != 0 {
                    value
                } else {
                    -1.0
                }
            };

            let current = number_for("Current Capacity");
            let max_cap = number_for("Max Capacity");
            if current < 0.0 {
                continue;
            }
            let mut level = current;
            if max_cap > 0.0 && max_cap != 100.0 {
                level = (current / max_cap) * 100.0;
            }
            if level > 1.0 {
                level /= 100.0;
            }
            if !(0.0..=1.0).contains(&level) {
                continue;
            }

            info.battery_level = level;
            info.form_factor = super::cstring_from("laptop");

            let state_key = cfstr("Power Source State");
            // SAFETY: `desc`/`state_key` are valid for this call.
            let state = unsafe { CFDictionaryGetValue(desc, state_key) };
            // SAFETY: `state_key` is owned, from `cfstr`.
            unsafe { CFRelease(state_key) };
            let charging_key = cfstr("Is Charging");
            // SAFETY: `desc`/`charging_key` are valid for this call.
            let charging = unsafe { CFDictionaryGetValue(desc, charging_key) };
            // SAFETY: `charging_key` is owned, from `cfstr`.
            unsafe { CFRelease(charging_key) };

            let is_charging = !charging.is_null() && {
                // SAFETY: `charging` was just checked non-null and is a live
                // CFBooleanRef from the dictionary.
                unsafe { CFBooleanGetValue(charging as CfBooleanRef) != 0 }
            };
            if is_charging {
                info.battery_state =
                    super::cstring_from(if level >= 0.999 { "full" } else { "charging" });
            } else if !state.is_null() {
                let ac = cfstr("AC Power");
                // SAFETY: `state` (non-null, from the dictionary) and `ac`
                // (owned, from `cfstr`) are both valid CFStringRefs.
                let cmp = unsafe { CFStringCompare(state as CfStringRef, ac, 0) };
                // SAFETY: `ac` is owned, from `cfstr`.
                unsafe { CFRelease(ac) };
                info.battery_state = super::cstring_from(if cmp == K_CF_COMPARE_EQUAL_TO {
                    if level >= 0.999 {
                        "full"
                    } else {
                        "charging"
                    }
                } else {
                    "unplugged"
                });
            } else {
                info.battery_state = super::cstring_from("unplugged");
            }
            break;
        }

        // SAFETY: `list` and `blob` are both valid, owned CF objects from the
        // two Copy calls above.
        unsafe {
            CFRelease(list);
            CFRelease(blob);
        }
    }

    pub(super) fn collect_device_info(info: &mut DeviceInfoState) {
        info.platform = super::cstring_from("macos");

        let mut model = sysctl_string("hw.model");
        if model.is_empty() {
            model = "Mac".to_string();
        }
        info.model = super::cstring_from(&model);

        let mut buf = [0u8; 256];
        // SAFETY: `buf` is a valid, correctly-sized stack buffer.
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut c_char, buf.len() - 1) };
        let name = if rc == 0 {
            // SAFETY: gethostname NUL-terminates on success within `buf`.
            let cstr = unsafe { CStr::from_ptr(buf.as_ptr() as *const c_char) };
            let text = cstr.to_string_lossy().into_owned();
            if text.is_empty() {
                model.clone()
            } else {
                text
            }
        } else {
            model.clone()
        };
        info.name = super::cstring_from(&name);

        let product_version = sysctl_string("kern.osproductversion");
        let os_version = if product_version.is_empty() {
            "macOS".to_string()
        } else {
            format!("macOS {product_version}")
        };
        info.os_version = super::cstring_from(&os_version);

        let mut chip = sysctl_string("machdep.cpu.brand_string");
        if chip.is_empty() {
            chip = "unknown".to_string();
        }
        info.chip = super::cstring_from(&chip);

        info.total_memory = sysctl_i64("hw.memsize");
        info.available_memory = macos_available_memory_bytes();

        let ncpu = sysctl_i64("hw.ncpu");
        info.core_count = if ncpu > 0 { ncpu as i32 } else { 1 };
        let perf = sysctl_i64("hw.perflevel0.logicalcpu");
        let eff = sysctl_i64("hw.perflevel1.logicalcpu");
        if perf > 0 {
            info.performance_cores = perf as i32;
            info.efficiency_cores = if eff > 0 { eff as i32 } else { 0 };
        } else {
            info.performance_cores = info.core_count;
            info.efficiency_cores = 0;
        }

        // SAFETY: `uts` is a zeroed, correctly-sized stack struct; uname only
        // writes into it.
        let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
        // SAFETY: `&mut uts` is a valid pointer to a `libc::utsname`.
        let architecture = if unsafe { libc::uname(&mut uts) } == 0 {
            // SAFETY: uname NUL-terminates `machine` on success.
            let cstr = unsafe { CStr::from_ptr(uts.machine.as_ptr()) };
            let text = cstr.to_string_lossy().into_owned();
            if text.is_empty() {
                "unknown".to_string()
            } else {
                text
            }
        } else {
            "unknown".to_string()
        };
        info.architecture = super::cstring_from(&architecture);

        info.form_factor = super::cstring_from(if model.contains("Book") {
            "laptop"
        } else {
            "desktop"
        });
        #[cfg(target_arch = "aarch64")]
        {
            info.gpu_family = super::cstring_from("apple");
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            info.gpu_family = super::cstring_from("unknown");
        }

        macos_sample_battery(info);
    }
}

// -----------------------------------------------------------------------------
// Windows
// -----------------------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use super::DeviceInfoState;
    use std::ffi::CStr;

    use windows_sys::Win32::Foundation::MAX_COMPUTERNAME_LENGTH;
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    use windows_sys::Win32::System::Registry::{RegGetValueA, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    use windows_sys::Win32::System::SystemInformation::{
        GetComputerNameA, GetNativeSystemInfo, GlobalMemoryStatusEx, MEMORYSTATUSEX,
        PROCESSOR_ARCHITECTURE_AMD64, PROCESSOR_ARCHITECTURE_ARM64, PROCESSOR_ARCHITECTURE_INTEL,
        SYSTEM_INFO,
    };

    pub(super) fn collect_device_info(info: &mut DeviceInfoState) {
        info.platform = super::cstring_from("windows");

        let mut computer_name = [0u8; MAX_COMPUTERNAME_LENGTH as usize + 1];
        let mut name_len = computer_name.len() as u32;
        // SAFETY: `computer_name`/`name_len` are a correctly-sized buffer and
        // its capacity; GetComputerNameA only writes within them.
        let ok = unsafe { GetComputerNameA(computer_name.as_mut_ptr(), &mut name_len) };
        let name = if ok != 0 {
            // SAFETY: GetComputerNameA NUL-terminates on success.
            unsafe { CStr::from_ptr(computer_name.as_ptr() as *const i8) }
                .to_string_lossy()
                .into_owned()
        } else {
            "Windows PC".to_string()
        };
        info.name = super::cstring_from(&name);
        info.model = super::cstring_from("Windows PC");
        info.os_version = super::cstring_from("Windows");

        let mut cpu_name = [0u8; 256];
        let mut cpu_name_size = cpu_name.len() as u32;
        let subkey = c"HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0";
        let value_name = c"ProcessorNameString";
        // SAFETY: `subkey`/`value_name` are valid NUL-terminated C strings;
        // `cpu_name`/`cpu_name_size` are a correctly-sized out buffer.
        let rc = unsafe {
            RegGetValueA(
                HKEY_LOCAL_MACHINE,
                subkey.as_ptr() as *const u8,
                value_name.as_ptr() as *const u8,
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                cpu_name.as_mut_ptr() as *mut core::ffi::c_void,
                &mut cpu_name_size,
            )
        };
        info.chip = if rc == 0 && cpu_name[0] != 0 {
            // SAFETY: RegGetValueA NUL-terminates a REG_SZ result on success.
            let text = unsafe { CStr::from_ptr(cpu_name.as_ptr() as *const i8) }
                .to_string_lossy()
                .into_owned();
            super::cstring_from(super::trim(&text))
        } else {
            super::cstring_from("unknown")
        };

        let mut mem: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: `mem` is zero-initialized with `dwLength` set per the
        // documented contract; GlobalMemoryStatusEx only writes within it.
        if unsafe { GlobalMemoryStatusEx(&mut mem) } != 0 {
            info.total_memory = mem.ullTotalPhys as i64;
            info.available_memory = mem.ullAvailPhys as i64;
        }

        let mut sys_info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: `sys_info` is a zeroed, correctly-sized stack struct.
        unsafe { GetNativeSystemInfo(&mut sys_info) };
        // SAFETY: reading a `union`'s populated `Anonymous.Anonymous` bitfield
        // view is the documented way to read `wProcessorArchitecture` after
        // GetNativeSystemInfo.
        let processor_architecture = unsafe { sys_info.Anonymous.Anonymous.wProcessorArchitecture };
        info.core_count = if sys_info.dwNumberOfProcessors > 0 {
            sys_info.dwNumberOfProcessors as i32
        } else {
            1
        };
        info.performance_cores = info.core_count;
        info.efficiency_cores = 0;
        info.architecture = super::cstring_from(match processor_architecture {
            PROCESSOR_ARCHITECTURE_AMD64 => "x86_64",
            PROCESSOR_ARCHITECTURE_ARM64 => "arm64",
            PROCESSOR_ARCHITECTURE_INTEL => "x86",
            _ => "unknown",
        });

        let mut power: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        // SAFETY: `power` is a zeroed, correctly-sized stack struct.
        let has_power = unsafe { GetSystemPowerStatus(&mut power) } != 0;
        if has_power && power.BatteryFlag != 128 && power.BatteryFlag != 255 {
            info.form_factor = super::cstring_from("laptop");
            if power.BatteryLifePercent <= 100 {
                info.battery_level = power.BatteryLifePercent as f64 / 100.0;
                info.battery_state = super::cstring_from(if power.ACLineStatus == 1 {
                    if power.BatteryLifePercent == 100 {
                        "full"
                    } else {
                        "charging"
                    }
                } else {
                    "unplugged"
                });
            }
        } else {
            info.form_factor = super::cstring_from("desktop");
        }
        info.gpu_family = super::cstring_from("unknown");
    }
}

fn collect_device_info(info: &mut DeviceInfoState) {
    platform::collect_device_info(info);
}

// -----------------------------------------------------------------------------
// Device-registration callbacks (extern "C", handed to the SDK)
// -----------------------------------------------------------------------------

// SAFETY (all five below): each is a `rac_device_*_fn` handed to
// `rac_device_manager_set_callbacks`; the SDK calls them synchronously and
// never across a panic boundary it understands, so every body is wrapped in
// `catch_unwind` and never lets a Rust panic cross the ABI.

unsafe extern "C" fn device_get_info(
    out_info: *mut sys::rac_device_registration_info_t,
    _user_data: *mut std::ffi::c_void,
) {
    let _ = std::panic::catch_unwind(|| {
        if out_info.is_null() {
            return;
        }
        let mut info = state().lock().unwrap_or_else(|e| e.into_inner());
        info.battery_level = -1.0;
        info.battery_state = CString::default();
        collect_device_info(&mut info);
        let fingerprint_input = format!(
            "{}|{}|{}|{}",
            info.model.to_string_lossy(),
            info.chip.to_string_lossy(),
            info.total_memory,
            info.core_count
        );
        info.fingerprint = cstring_from(sha256_hex(&fingerprint_input));

        // SAFETY: `out_info` was just checked non-null; it points to a valid,
        // writable `rac_device_registration_info_t` for the duration of this
        // call (the SDK's documented contract for this callback).
        unsafe {
            *out_info = sys::rac_device_registration_info_t {
                device_id: info.device_id.as_ptr(),
                device_model: info.model.as_ptr(),
                device_name: info.name.as_ptr(),
                platform: info.platform.as_ptr(),
                os_version: info.os_version.as_ptr(),
                form_factor: info.form_factor.as_ptr(),
                architecture: info.architecture.as_ptr(),
                chip_name: info.chip.as_ptr(),
                total_memory: info.total_memory,
                available_memory: info.available_memory,
                has_neural_engine: sys::FALSE,
                neural_engine_cores: 0,
                gpu_family: info.gpu_family.as_ptr(),
                battery_level: info.battery_level,
                battery_state: if info.battery_state.as_bytes().is_empty() {
                    std::ptr::null()
                } else {
                    info.battery_state.as_ptr()
                },
                is_low_power_mode: sys::FALSE,
                core_count: info.core_count,
                performance_cores: info.performance_cores,
                efficiency_cores: info.efficiency_cores,
                device_fingerprint: info.fingerprint.as_ptr(),
            };
        }
    });
}

unsafe extern "C" fn device_get_id(
    _user_data: *mut std::ffi::c_void,
) -> *const std::os::raw::c_char {
    std::panic::catch_unwind(|| {
        state()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .device_id
            .as_ptr()
    })
    .unwrap_or(std::ptr::null())
}

unsafe extern "C" fn device_is_registered(_user_data: *mut std::ffi::c_void) -> sys::rac_bool_t {
    std::panic::catch_unwind(|| {
        if state().lock().unwrap_or_else(|e| e.into_inner()).registered {
            sys::TRUE
        } else {
            sys::FALSE
        }
    })
    .unwrap_or(sys::FALSE)
}

unsafe extern "C" fn device_set_registered(
    registered: sys::rac_bool_t,
    _user_data: *mut std::ffi::c_void,
) {
    let _ = std::panic::catch_unwind(|| {
        state().lock().unwrap_or_else(|e| e.into_inner()).registered = registered == sys::TRUE;
    });
}

/// Same control-plane POST shape as `bootstrap::wally_telemetry_http_callback`:
/// commons base URL + relative endpoint over the registered desktop HTTP
/// transport, bearer token attached when the auth manager holds one.
unsafe extern "C" fn device_http_post(
    endpoint: *const std::os::raw::c_char,
    json_body: *const std::os::raw::c_char,
    requires_auth: sys::rac_bool_t,
    out_response: *mut sys::rac_device_http_response_t,
    _user_data: *mut std::ffi::c_void,
) -> sys::rac_result_t {
    std::panic::catch_unwind(|| {
        let mut info = state().lock().unwrap_or_else(|e| e.into_inner());
        info.http_body = CString::default();
        info.http_error = CString::default();

        let fail = |info: &mut DeviceInfoState,
                    rc: sys::rac_result_t,
                    message: &str|
         -> sys::rac_result_t {
            info.http_error = cstring_from(message);
            if !out_response.is_null() {
                // SAFETY: `out_response` was checked non-null; it points to a
                // valid, writable `rac_device_http_response_t` for the
                // duration of this call.
                unsafe {
                    (*out_response).result = rc;
                    (*out_response).status_code = 0;
                    (*out_response).response_body = std::ptr::null();
                    (*out_response).error_message = info.http_error.as_ptr();
                }
            }
            rc
        };

        if endpoint.is_null() || json_body.is_null() {
            return fail(
                &mut info,
                sys::RAC_ERROR_INVALID_ARGUMENT,
                "invalid registration request",
            );
        }

        // SAFETY: `endpoint`/`json_body` were just checked non-null and are
        // NUL-terminated for the duration of this call (the SDK's documented
        // contract for this callback).
        let endpoint_str = unsafe { CStr::from_ptr(endpoint) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: see above.
        let json_body_bytes = unsafe { CStr::from_ptr(json_body) }.to_bytes();

        // SAFETY: rac_state_get_base_url() returns a commons-owned
        // NUL-terminated string (or NULL) that stays valid for this call.
        let base_url_ptr = unsafe { sys::rac_state_get_base_url() };
        let base_url = if base_url_ptr.is_null() {
            String::new()
        } else {
            // SAFETY: non-null, NUL-terminated per rac_state_get_base_url's contract.
            unsafe { CStr::from_ptr(base_url_ptr) }
                .to_string_lossy()
                .into_owned()
        };
        // SAFETY: takes no arguments; always safe to call.
        if base_url.is_empty() || unsafe { sys::rac_http_transport_is_registered() } != sys::TRUE {
            return fail(
                &mut info,
                sys::RAC_ERROR_NETWORK_ERROR,
                "device registration transport unavailable",
            );
        }

        let mut url_buf = [0 as std::os::raw::c_char; 2048];
        let base_url_c = cstring_from(&base_url);
        // SAFETY: `base_url_c`/endpoint are valid NUL-terminated strings;
        // `url_buf` is a correctly-sized out buffer.
        let written = unsafe {
            sys::rac_build_url(
                base_url_c.as_ptr(),
                endpoint,
                url_buf.as_mut_ptr(),
                url_buf.len(),
            )
        };
        if written < 0 {
            return fail(
                &mut info,
                sys::RAC_ERROR_NETWORK_ERROR,
                "device registration URL build failed",
            );
        }

        let mut headers: Vec<sys::rac_http_header_kv_t> = Vec::new();
        let mut defaults_ptr: *const sys::rac_http_header_kv_t = std::ptr::null();
        let mut default_count: usize = 0;
        // SAFETY: out-params are valid stack locals.
        if unsafe { sys::rac_http_default_headers(&mut defaults_ptr, &mut default_count) }
            == sys::SUCCESS
            && !defaults_ptr.is_null()
        {
            // SAFETY: commons guarantees `defaults_ptr` describes
            // `default_count` valid, static-lifetime entries.
            let defaults = unsafe { std::slice::from_raw_parts(defaults_ptr, default_count) };
            headers.extend_from_slice(defaults);
        }
        let mut auth_value = CString::default();
        if requires_auth == sys::TRUE {
            // SAFETY: takes no arguments; returns a commons-owned
            // NUL-terminated string or NULL, valid for this call.
            let token_ptr = unsafe { sys::rac_auth_get_access_token() };
            if !token_ptr.is_null() {
                // SAFETY: non-null, NUL-terminated per the function's contract.
                let token = unsafe { CStr::from_ptr(token_ptr) }.to_string_lossy();
                if !token.is_empty() {
                    auth_value = cstring_from(format!("Bearer {token}"));
                }
            }
        }
        let auth_name = c"Authorization";
        if !auth_value.as_bytes().is_empty() {
            headers.push(sys::rac_http_header_kv_t {
                name: auth_name.as_ptr(),
                value: auth_value.as_ptr(),
            });
        }

        let mut client: *mut sys::rac_http_client_t = std::ptr::null_mut();
        // SAFETY: `&mut client` is a valid out-param.
        if unsafe { sys::rac_http_client_create(&mut client) } != sys::SUCCESS {
            return fail(
                &mut info,
                sys::RAC_ERROR_NETWORK_ERROR,
                "device registration client create failed",
            );
        }

        // SAFETY: takes no arguments; always safe to call.
        let environment = unsafe { sys::rac_state_get_environment() };
        let request = sys::rac_http_request_t {
            method: c"POST".as_ptr(),
            url: url_buf.as_ptr(),
            headers: if headers.is_empty() {
                std::ptr::null()
            } else {
                headers.as_ptr()
            },
            header_count: headers.len(),
            body_bytes: json_body_bytes.as_ptr(),
            body_len: json_body_bytes.len(),
            // SAFETY: takes an environment enum; always safe to call.
            timeout_ms: unsafe { sys::rac_env_default_http_timeout_ms(environment) },
            follow_redirects: sys::FALSE,
            expected_checksum_hex: std::ptr::null(),
        };

        let mut response: sys::rac_http_response_t = unsafe { std::mem::zeroed() };
        // SAFETY: `client` is a freshly created, valid handle; `request` is
        // built above with all pointers alive for this call; `response` is a
        // correctly-sized out-param.
        let rc = unsafe { sys::rac_http_request_send(client, &request, &mut response) };
        // SAFETY: `client` was created by rac_http_client_create above and is
        // not used again after this.
        unsafe { sys::rac_http_client_destroy(client) };

        if !response.body_bytes.is_null() && response.body_len > 0 {
            // SAFETY: commons guarantees `body_bytes` holds `body_len` valid
            // bytes until `rac_http_response_free`.
            let body_slice =
                unsafe { std::slice::from_raw_parts(response.body_bytes, response.body_len) };
            info.http_body = CString::new(body_slice.to_vec()).unwrap_or_default();
        }
        let status = response.status;
        // SAFETY: `response` was populated by rac_http_request_send above
        // (or left zeroed on early failure), both valid to free.
        unsafe { sys::rac_http_response_free(&mut response) };

        let ok = rc == sys::SUCCESS && (200..300).contains(&status);
        if !ok {
            info.http_error =
                cstring_from(format!("device registration POST failed (http {status})"));
        }
        let result = if ok {
            sys::SUCCESS
        } else {
            sys::RAC_ERROR_NETWORK_ERROR
        };
        if !out_response.is_null() {
            // SAFETY: `out_response` was checked non-null above; valid,
            // writable for the duration of this call.
            unsafe {
                (*out_response).result = result;
                (*out_response).status_code = status;
                (*out_response).response_body = if info.http_body.as_bytes().is_empty() {
                    std::ptr::null()
                } else {
                    info.http_body.as_ptr()
                };
                (*out_response).error_message = if ok {
                    std::ptr::null()
                } else {
                    info.http_error.as_ptr()
                };
            }
        }
        let _ = endpoint_str; // parsed above only to validate non-null contract
        result
    })
    .unwrap_or(sys::RAC_ERROR_NETWORK_ERROR)
}

/// `runanywhere::sha256_hex` — lowercase hex SHA-256, matching the C++
/// `rac/foundation/rac_sha256.h` helper exactly (same algorithm, same input
/// bytes → same digest text).
fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(digest)
}

use std::ffi::CStr;

#[cfg(test)]
mod parse_leading_i64_tests {
    use super::parse_leading_i64;

    #[test]
    fn meminfo_kb_suffix_is_ignored_like_strtoll() {
        // C++'s strtoll(line.c_str() + prefix.size(), nullptr, 10) parses the
        // leading digit run of a real /proc/meminfo remainder and ignores the
        // trailing " kB" unit; a naive `str::parse::<i64>()` on the whole
        // trimmed remainder rejects it outright and previously made
        // meminfo_bytes() always return 0.
        assert_eq!(parse_leading_i64("16130004 kB"), 16130004);
        assert_eq!(parse_leading_i64("       16130004 kB"), 16130004);
    }

    #[test]
    fn leading_whitespace_is_skipped() {
        assert_eq!(parse_leading_i64("   42"), 42);
    }

    #[test]
    fn no_leading_digits_returns_zero() {
        assert_eq!(parse_leading_i64("abc"), 0);
        assert_eq!(parse_leading_i64(""), 0);
    }

    #[test]
    fn leading_sign_is_honored() {
        assert_eq!(parse_leading_i64("-5 kB"), -5);
        assert_eq!(parse_leading_i64("+7 kB"), 7);
    }
}
