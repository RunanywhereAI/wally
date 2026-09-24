//! The stored console credential, console/browser origin rules and profile
//! paths (port of src/account/credentials.cpp). Owner: the account port.

use crate::util::getenv;

/// The credential is a normal API key with the customer's credit behind it:
/// `Debug` never prints the tokens.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub console_url: String,
    pub email: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("console_url", &self.console_url)
            .field("email", &self.email)
            .field(
                "access_token",
                &if self.access_token.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field(
                "refresh_token",
                &if self.refresh_token.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Credentials {
    pub fn signed_in(&self) -> bool {
        !self.access_token.is_empty()
    }

    /// `skew_seconds` defaulted to 60 in C++.
    pub fn access_token_expired(&self, now: i64, skew_seconds: i64) -> bool {
        self.expires_at > 0 && self.expires_at <= now + skew_seconds
    }
}

// Two hosts, two jobs, two different deployments. Keep them adjacent: the bug
// they replace was one constant doing both, and it broke every command that
// talks to the console.
//
// The API is the control plane -- /auth/cli/start, /auth/cli/poll,
// /auth/cli/refresh, /v1/me, /v1/cli/* -- and it is served by Cloud Run. The web
// console is the page a person approves a sign-in on, and it is served by
// Railway. Measured 2026-09-04:
//
//   inference.runanywhere.ai  /auth/cli/start 422  /v1/me 405  Google Frontend
//   console.runanywhere.ai    /auth/cli/start 404  /v1/me 404  railway-hikari
//
// 422 and 405 are those endpoints rejecting a bad body and a wrong verb, which
// is how you know they are there. 404 with the console's own SPA HTML is how
// you know they are not. Point the API constant at the Railway host -- which is
// what it used to be -- and login, whoami and usage all 404.
const PRODUCTION_CONSOLE_API: &str = "https://inference.runanywhere.ai";

// The approval page, under both names it answers to. One deployment: measured
// 2026-09-04, both return `server: railway-hikari` and the same `etag:
// "nqkdmw"` for /cloud/cli, so this is two DNS names for one build and not two
// origins' worth of trust.
//
// The raw Railway name is here because it is what the control plane actually
// hands out today:
//
//   POST https://inference.runanywhere.ai/auth/cli/start
//     -> verification_url: https://runanywhere-frontend-production.up.railway.app/cloud/cli?code=...
//
// Trusting only the custom domain would refuse every real sign-in. Delete the
// Railway entry once the control plane's configured console origin is the
// custom domain -- that is a one-line config change on the API side, and this
// list is the thing waiting on it.
const PRODUCTION_CONSOLE_WEB: [&str; 2] = [
    "https://console.runanywhere.ai",
    "https://runanywhere-frontend-production.up.railway.app",
];

const MAXIMUM_CREDENTIAL_BYTES: u64 = 1024 * 1024;

#[cfg(windows)]
const FILE_NAME: &str = "credentials.dat";
#[cfg(not(windows))]
const FILE_NAME: &str = "credentials.json";

/// `name` (the current, documented variable) wins whenever it's set. `legacy`
/// is read only when `name` is unset, so an installed rcli-era override still
/// works after the wally rename -- with a one-line notice, since it's a name
/// nobody should be setting a year from now.
fn env_with_legacy_fallback(name: &str, legacy: &str) -> String {
    if let Some(value) = getenv(name) {
        return value;
    }
    if let Some(legacy_value) = getenv(legacy) {
        eprintln!("warning: {legacy} is deprecated, use {name} instead");
        return legacy_value;
    }
    String::new()
}

fn has_unsafe_character(text: &str) -> bool {
    text.bytes().any(|c| c <= 0x20 || c == 0x7f || c == b'\\')
}

#[derive(Debug, Clone, Default)]
struct ParsedUrl {
    scheme: String,
    host: String,
    port: String,
    suffix: String,
    bracketed: bool,
}

/// Reduces a console URL's path to the prefix every endpoint hangs off, or
/// fails if it is not one.
///
/// A console is not always at the root of its host. Development is reached at
/// `https://inference.runanywhere.ai/api-dev`, where the load balancer strips
/// the prefix and forwards to the dev control plane; the same host without it
/// is production. Refusing the path -- which this did until it was found --
/// leaves no way to name the dev console at all, so `--console-url` had to be
/// given the backend's own Cloud Run hostname, which bypasses the load balancer
/// and therefore reaches different code than any real client does.
///
/// A query or fragment is still refused. Every caller builds an endpoint by
/// appending to this string, and `?a=1` + `/v1/me` is not a URL.
fn normalize_base_path(suffix: &str) -> Option<String> {
    if suffix.is_empty() || suffix == "/" {
        return Some(String::new());
    }
    if !suffix.starts_with('/')
        || suffix.contains(['?', '#'])
        || suffix.contains("//")
        || suffix.contains("/.")
        || suffix.len() > 256
    {
        return None;
    }
    Some(if let Some(stripped) = suffix.strip_suffix('/') {
        stripped.to_string()
    } else {
        suffix.to_string()
    })
}

fn valid_port(port: &str) -> bool {
    if port.is_empty() {
        return true;
    }
    if !port.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match port.parse::<u32>() {
        Ok(value) => value > 0 && value <= 65535,
        Err(_) => false,
    }
}

fn valid_host(host: &str, bracketed: bool) -> bool {
    if host.is_empty() || host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    if bracketed {
        host.bytes()
            .all(|c| c.is_ascii_hexdigit() || c == b':' || c == b'.')
    } else {
        host.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
    }
}

fn parse_url(input: &str, allow_suffix: bool) -> Option<ParsedUrl> {
    if input.is_empty() || has_unsafe_character(input) {
        return None;
    }
    let scheme_end = input.find("://")?;
    let scheme = input[..scheme_end].to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }

    let authority_start = scheme_end + 3;
    let rest = &input[authority_start..];
    let authority_end = rest.find(['/', '?', '#']);
    let authority = match authority_end {
        Some(i) => &rest[..i],
        None => rest,
    };
    let suffix = match authority_end {
        Some(i) => rest[i..].to_string(),
        None => String::new(),
    };
    if authority.is_empty()
        || authority.contains('@')
        || (!allow_suffix && !suffix.is_empty() && suffix != "/")
    {
        return None;
    }

    let host;
    let mut port = String::new();
    let bracketed;
    if let Some(inner) = authority.strip_prefix('[') {
        let close = inner.find(']')?;
        bracketed = true;
        host = inner[..close].to_ascii_lowercase();
        let remainder = &inner[close + 1..];
        if !remainder.is_empty() {
            if !remainder.starts_with(':') || remainder.len() == 1 {
                return None;
            }
            port = remainder[1..].to_string();
        }
    } else {
        if authority.matches(':').count() > 1 {
            return None;
        }
        bracketed = false;
        match authority.rfind(':') {
            Some(colon) => {
                host = authority[..colon].to_ascii_lowercase();
                port = authority[colon + 1..].to_string();
                if port.is_empty() {
                    return None;
                }
            }
            None => host = authority.to_ascii_lowercase(),
        }
    }
    if !valid_host(&host, bracketed) || !valid_port(&port) {
        return None;
    }

    let loopback = host == "localhost" || host == "127.0.0.1" || (bracketed && host == "::1");
    if scheme == "http" && !loopback {
        return None;
    }
    Some(ParsedUrl {
        scheme,
        host,
        port,
        suffix,
        bracketed,
    })
}

fn render_origin(url: &ParsedUrl) -> String {
    let mut rendered = format!("{}://", url.scheme);
    if url.bracketed {
        rendered.push('[');
        rendered.push_str(&url.host);
        rendered.push(']');
    } else {
        rendered.push_str(&url.host);
    }
    if !url.port.is_empty() {
        rendered.push(':');
        rendered.push_str(&url.port);
    }
    rendered
}

#[cfg(windows)]
fn home_directory() -> String {
    if let Some(local) = getenv("LOCALAPPDATA") {
        return local;
    }
    getenv("USERPROFILE").unwrap_or_default()
}

#[cfg(not(windows))]
fn home_directory() -> String {
    if let Some(home) = getenv("HOME") {
        return home;
    }
    // SAFETY: getpwuid returns either null or a pointer into storage owned by
    // libc; pw_dir is read immediately and copied out before any other libc
    // call could invalidate it.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if entry.is_null() {
            return String::new();
        }
        let pw_dir = (*entry).pw_dir;
        if pw_dir.is_null() {
            return String::new();
        }
        std::ffi::CStr::from_ptr(pw_dir)
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(not(windows))]
fn ensure_secure_directory(directory: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let path = std::path::Path::new(directory);
    if let Ok(before) = std::fs::symlink_metadata(path) {
        if before.file_type().is_symlink() {
            return Err("credential directory may not be a symbolic link".to_string());
        }
    }
    std::fs::create_dir_all(path)
        .map_err(|_| "could not create the credential directory".to_string())?;
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        _ => return Err("could not create the credential directory".to_string()),
    }
    // SAFETY: `cpath` is a NUL-terminated C string built from `directory`;
    // chmod takes that pointer and a plain integer mode, nothing escapes.
    let cpath = std::ffi::CString::new(directory)
        .map_err(|_| "could not restrict the credential directory".to_string())?;
    let rc = unsafe { libc::chmod(cpath.as_ptr(), libc::S_IRWXU as libc::mode_t) };
    if rc != 0 {
        return Err("could not restrict the credential directory".to_string());
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| "credential directory has unsafe ownership".to_string())?;
    // SAFETY: geteuid takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_dir() || metadata.uid() != euid {
        return Err("credential directory has unsafe ownership".to_string());
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_secure_directory(directory: &str) -> Result<(), String> {
    let path = std::path::Path::new(directory);
    if let Ok(before) = std::fs::symlink_metadata(path) {
        if before.file_type().is_symlink() {
            return Err("credential directory may not be a symbolic link".to_string());
        }
    }
    std::fs::create_dir_all(path)
        .map_err(|_| "could not create the credential directory".to_string())?;
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        _ => Err("could not create the credential directory".to_string()),
    }
}

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTOAPI_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
    };

    /// Zero `bytes` in place without the compiler eliding the writes -- the
    /// portable equivalent of Windows' `SecureZeroMemory`.
    fn secure_zero(bytes: &mut [u8]) {
        for b in bytes.iter_mut() {
            // SAFETY: `b` is a valid `&mut u8` from the slice we are iterating.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }

    pub fn protect(plaintext: &[u8]) -> Result<Vec<u8>, String> {
        if plaintext.len() > u32::MAX as usize {
            return Err("credential document is too large".to_string());
        }
        let description: Vec<u16> = "RunAnywhere Wally cloud session\0".encode_utf16().collect();
        let mut input = CRYPTOAPI_BLOB {
            cbData: plaintext.len() as u32,
            pbData: plaintext.as_ptr() as *mut u8,
        };
        let mut output = CRYPTOAPI_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: `input` points at `plaintext`, which outlives the call;
        // `output` is a valid out-parameter CryptProtectData fills in and that
        // we free with LocalFree on success.
        let ok = unsafe {
            CryptProtectData(
                &mut input,
                description.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err("Windows could not protect the cloud session".to_string());
        }
        // SAFETY: CryptProtectData succeeded, so `output.pbData` points at
        // `output.cbData` valid bytes allocated by LocalAlloc, freed below.
        let bytes =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        // SAFETY: `output.pbData` was allocated by CryptProtectData with
        // LocalAlloc and is freed exactly once, here.
        unsafe { LocalFree(output.pbData as isize) };
        Ok(bytes)
    }

    pub fn unprotect(protected_bytes: &[u8]) -> Result<Vec<u8>, String> {
        if protected_bytes.len() > u32::MAX as usize {
            return Err("credential document is too large".to_string());
        }
        let mut input = CRYPTOAPI_BLOB {
            cbData: protected_bytes.len() as u32,
            pbData: protected_bytes.as_ptr() as *mut u8,
        };
        let mut output = CRYPTOAPI_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        // SAFETY: `input` points at `protected_bytes`, which outlives the
        // call; `output` is a valid out-parameter.
        let ok = unsafe {
            CryptUnprotectData(
                &mut input,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err("Windows could not unlock the cloud session".to_string());
        }
        // SAFETY: CryptUnprotectData succeeded, so `output.pbData` points at
        // `output.cbData` valid bytes.
        let mut bytes =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        secure_zero(unsafe {
            std::slice::from_raw_parts_mut(output.pbData, output.cbData as usize)
        });
        // SAFETY: `output.pbData` was allocated by CryptUnprotectData with
        // LocalAlloc and is freed exactly once, here, after it was copied out
        // and zeroed above.
        unsafe { LocalFree(output.pbData as isize) };
        if bytes.is_empty() {
            bytes.clear();
        }
        Ok(bytes)
    }
}

#[cfg(windows)]
fn read_document(path: &str) -> Result<Option<String>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("could not read the cloud session".to_string()),
    };
    if bytes.len() as u64 > MAXIMUM_CREDENTIAL_BYTES {
        return Err("protected cloud session exceeds the safety limit".to_string());
    }
    let plaintext = dpapi::unprotect(&bytes)?;
    String::from_utf8(plaintext)
        .map(Some)
        .map_err(|_| "could not read the cloud session".to_string())
}

#[cfg(windows)]
fn write_document(path: &str, document: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileA, DeleteFileA, FlushFileBuffers, MoveFileExA, WriteFile, FILE_ATTRIBUTE_HIDDEN,
        FILE_ATTRIBUTE_NOT_CONTENT_INDEXED, FILE_GENERIC_WRITE, MOVE_FILE_REPLACE_EXISTING,
        MOVE_FILE_WRITE_THROUGH,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    let protected = dpapi::protect(document.as_bytes())?;

    // SAFETY: GetCurrentProcessId takes no arguments and cannot fail.
    let pid = unsafe { GetCurrentProcessId() };
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let temporary = format!("{path}.tmp.{pid}.{tick}");
    let Ok(temporary_c) = std::ffi::CString::new(temporary.clone()) else {
        return Err("could not create the protected cloud session".to_string());
    };
    // SAFETY: `temporary_c` is a valid NUL-terminated C string; CreateFileA
    // does not retain the pointer past the call.
    let handle = unsafe {
        CreateFileA(
            temporary_c.as_ptr() as *const u8,
            FILE_GENERIC_WRITE.0,
            0,
            std::ptr::null_mut(),
            windows_sys::Win32::Storage::FileSystem::CREATE_NEW,
            FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
            0,
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err("could not create the protected cloud session".to_string());
    }
    let mut written: u32 = 0;
    // SAFETY: `handle` is the file handle just opened; `protected` outlives
    // the call and `written` is a valid out-parameter.
    let wrote = unsafe {
        WriteFile(
            handle,
            protected.as_ptr(),
            protected.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        ) != 0
            && written as usize == protected.len()
            && FlushFileBuffers(handle) != 0
    };
    // SAFETY: `handle` was returned by CreateFileA above and is closed
    // exactly once.
    unsafe { CloseHandle(handle) };
    let Ok(path_c) = std::ffi::CString::new(path) else {
        // SAFETY: `temporary_c` names the file just created; deleting it on
        // this failure path is best-effort cleanup.
        unsafe { DeleteFileA(temporary_c.as_ptr() as *const u8) };
        return Err("could not store the protected cloud session".to_string());
    };
    // SAFETY: both C strings are valid and outlive the call.
    let moved = wrote != false
        && unsafe {
            MoveFileExA(
                temporary_c.as_ptr() as *const u8,
                path_c.as_ptr() as *const u8,
                MOVE_FILE_REPLACE_EXISTING | MOVE_FILE_WRITE_THROUGH,
            ) != 0
        };
    if !moved {
        // SAFETY: `temporary_c` names the file just created; deleting it on
        // this failure path is best-effort cleanup.
        unsafe { DeleteFileA(temporary_c.as_ptr() as *const u8) };
        return Err("could not store the protected cloud session".to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
fn read_document(path: &str) -> Result<Option<String>, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut flags = 0;
        #[cfg(target_os = "linux")]
        {
            flags |= libc::O_CLOEXEC;
        }
        flags |= libc::O_NOFOLLOW;
        options.custom_flags(flags);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("could not securely read the cloud session".to_string()),
    };
    let metadata = file
        .metadata()
        .map_err(|_| "cloud session has unsafe file metadata".to_string())?;
    // SAFETY: geteuid takes no arguments and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.uid() != euid
        || metadata.nlink() != 1
        || metadata.size() > MAXIMUM_CREDENTIAL_BYTES
    {
        return Err("cloud session has unsafe file metadata".to_string());
    }
    // A prior version, another tool, or a permissive umask may have left this
    // group- or world-readable; the bearer token inside is as good as a
    // password, so say so once rather than quietly tightening it and leaving
    // the reader to wonder whether it was ever exposed.
    let was_exposed = metadata.mode() & 0o077 != 0;
    // SAFETY: `fd` is the raw descriptor of `file`, still open; fchmod does
    // not retain it past the call.
    let rc = unsafe { libc::fchmod(file.as_raw_fd(), 0o600) };
    if rc != 0 {
        return Err("cloud session has unsafe file metadata".to_string());
    }
    if was_exposed {
        eprintln!(
            "warning: {path} was readable beyond your user; permissions have been restricted \
             to your user only"
        );
    }
    use std::io::Read;
    let mut file = file;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)
        .map_err(|_| "could not read the cloud session".to_string())?;
    if buffer.len() as u64 > MAXIMUM_CREDENTIAL_BYTES {
        return Err("cloud session exceeds the safety limit".to_string());
    }
    let body =
        String::from_utf8(buffer).map_err(|_| "could not read the cloud session".to_string())?;
    Ok(Some(body))
}

#[cfg(not(windows))]
fn write_document(path: &str, document: &str) -> Result<(), String> {
    use std::io::Write;

    let directory = std::path::Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let pattern = format!("{directory}/.credentials.tmp.XXXXXX");
    let mut template = std::ffi::CString::new(pattern)
        .map_err(|_| "could not create a temporary cloud session".to_string())?
        .into_bytes_with_nul();
    // SAFETY: `template` is a mutable, NUL-terminated buffer ending in
    // "XXXXXX" as mkstemp requires; it fills the X's in place and returns an
    // owned fd on success.
    let fd = unsafe { libc::mkstemp(template.as_mut_ptr() as *mut libc::c_char) };
    if fd < 0 {
        return Err("could not create a temporary cloud session".to_string());
    }
    let temporary_path = std::ffi::CStr::from_bytes_with_nul(&template)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default();
    // SAFETY: `fd` was just returned by mkstemp and is owned by this `File`
    // for the rest of the function.
    let mut file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };

    let cleanup_and_fail = |temporary_path: &str, message: &str| -> Result<(), String> {
        let _ = std::fs::remove_file(temporary_path);
        Err(message.to_string())
    };

    // SAFETY: `fd` is the descriptor just opened by mkstemp, still valid.
    if unsafe { libc::fchmod(fd, 0o600) } != 0 {
        return cleanup_and_fail(
            &temporary_path,
            "could not atomically store the cloud session",
        );
    }
    if file.write_all(document.as_bytes()).is_err() {
        return cleanup_and_fail(
            &temporary_path,
            "could not atomically store the cloud session",
        );
    }
    if file.sync_all().is_err() {
        return cleanup_and_fail(
            &temporary_path,
            "could not atomically store the cloud session",
        );
    }
    drop(file);
    if std::fs::rename(&temporary_path, path).is_err() {
        return cleanup_and_fail(
            &temporary_path,
            "could not atomically store the cloud session",
        );
    }
    Ok(())
}

/// The console API used when neither a login flag nor WALLY_CONSOLE_URL is set.
pub fn default_console_url() -> String {
    let configured = env_with_legacy_fallback("WALLY_CONSOLE_URL", "RCLI_CONSOLE_URL");
    if !configured.is_empty() {
        return configured;
    }
    // A dev build carries its control plane compiled in (see
    // baked_endpoints.h.in): the env override above still wins, production
    // builds generate an empty macro and fall through unchanged.
    let baked = env!("WALLY_BAKED_CONSOLE_API_URL");
    if !baked.is_empty() {
        return baked.to_string();
    }
    PRODUCTION_CONSOLE_API.to_string()
}

/// The baked development control plane, normalised, or empty when none baked.
pub fn baked_console_api_url() -> String {
    let baked = env!("WALLY_BAKED_CONSOLE_API_URL");
    if baked.is_empty() {
        return String::new();
    }
    normalize_console_url(baked).unwrap_or_default()
}

/// The browser origins allowed to host the approval page for `console_url`.
/// Never empty.
pub fn trusted_browser_origins(console_url: &str) -> Vec<String> {
    // An operator who declared one has said which console they trust, and that
    // is then the only one.
    let declared = env_with_legacy_fallback("WALLY_CONSOLE_WEB_URL", "RCLI_CONSOLE_WEB_URL");
    if !declared.is_empty() {
        if let Ok(normalized) = normalize_console_url(&declared) {
            return vec![normalized];
        }
    }
    if console_url == PRODUCTION_CONSOLE_API {
        return PRODUCTION_CONSOLE_WEB
            .iter()
            .map(|s| s.to_string())
            .collect();
    }
    // A dev build that baked its control plane also baked which browser
    // console may approve sign-ins for it (a local console on loopback,
    // typically). Trust holds pairwise: the baked web origin is honored only
    // while talking to the baked API, never for an arbitrary --base-url.
    let baked_web = env!("WALLY_BAKED_CONSOLE_WEB_ORIGIN");
    let baked_api = baked_console_api_url();
    if !baked_web.is_empty() && !baked_api.is_empty() && console_url == baked_api {
        if let Ok(normalized) = normalize_console_url(baked_web) {
            return vec![normalized, console_url.to_string()];
        }
    }
    // Anything else -- a dev console, a loopback stub -- is trusted only at its
    // own origin. That is the rule that held before, and it is the safe answer
    // for a console whose approval page we know nothing about.
    vec![console_url.to_string()]
}

pub fn browser_url_is_trusted(url: &str, origins: &[String]) -> bool {
    origins
        .iter()
        .any(|origin| browser_url_matches_console(url, origin))
}

/// Validate and canonicalize a console origin. HTTPS is required except for an
/// exact loopback host. Origins may not contain credentials, paths or queries.
pub fn normalize_console_url(input: &str) -> Result<String, String> {
    const MESSAGE: &str = "console URL must be an HTTPS origin with an optional path, or HTTP \
                            on exact loopback";
    let parsed = parse_url(input, true).ok_or_else(|| MESSAGE.to_string())?;
    let base_path = normalize_base_path(&parsed.suffix).ok_or_else(|| MESSAGE.to_string())?;
    Ok(render_origin(&parsed) + &base_path)
}

/// Browser URLs may include a path/query but obey the same HTTPS/loopback rule.
pub fn browser_url_is_safe(url: &str) -> bool {
    parse_url(url, true).is_some()
}

pub fn browser_url_matches_console(url: &str, console_url: &str) -> bool {
    // Origins, not full URLs: a console's base path says where its API lives,
    // and says nothing about which pages on that host may approve a sign-in.
    match (parse_url(url, true), parse_url(console_url, true)) {
        (Some(browser), Some(console)) => render_origin(&browser) == render_origin(&console),
        _ => false,
    }
}

/// Bearer/refresh tokens are constrained to RFC 6750 b64token characters.
pub fn session_token_is_safe(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 8192
        && token.bytes().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
        })
}

pub fn profile_directory() -> String {
    let override_dir = env_with_legacy_fallback("WALLY_PROFILE_DIR", "RCLI_PROFILE_DIR");
    if !override_dir.is_empty() {
        return override_dir;
    }
    #[cfg(windows)]
    {
        let home = home_directory();
        if home.is_empty() {
            String::new()
        } else {
            format!("{home}/RunAnywhere/Wally")
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = getenv("XDG_CONFIG_HOME") {
            return format!("{xdg}/wally");
        }
        let home = home_directory();
        if home.is_empty() {
            String::new()
        } else {
            format!("{home}/.config/wally")
        }
    }
}

pub fn credentials_path() -> String {
    let directory = profile_directory();
    if directory.is_empty() {
        String::new()
    } else {
        format!("{directory}/{FILE_NAME}")
    }
}

/// Missing credentials are not an error; the result then carries the default
/// console URL. (C++ `bool Load(Credentials*, std::string*)`.)
pub fn load() -> Result<Credentials, String> {
    let mut credentials = Credentials {
        console_url: normalize_console_url(&default_console_url())?,
        ..Credentials::default()
    };

    let path = credentials_path();
    if path.is_empty() {
        return Err("no user profile directory is available".to_string());
    }
    ensure_secure_directory(&profile_directory())?;
    let Some(document) = read_document(&path)? else {
        return Ok(credentials);
    };

    let object: serde_json::Value = serde_json::from_str(&document)
        .map_err(|_| "cloud session file is not valid JSON".to_string())?;
    let Some(map) = object.as_object() else {
        return Err("cloud session file is not a JSON object".to_string());
    };

    // A missing or empty console_url is not a malformed one: `credentials`
    // already carries the default-console-derived origin set above, so only a
    // value the document actually specifies gets validated. Without this, a
    // hand-edited or partially-written file that simply omits the field fails
    // with "console URL must be HTTPS" instead of falling back.
    let stored_url = map
        .get("console_url")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !stored_url.is_empty() {
        credentials.console_url = normalize_console_url(stored_url)?;
    }
    credentials.email = map
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let access_token = map
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let refresh_token = map
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if (!access_token.is_empty() && !session_token_is_safe(&access_token))
        || (!refresh_token.is_empty() && !session_token_is_safe(&refresh_token))
    {
        return Err("cloud session contains an invalid token encoding".to_string());
    }
    credentials.access_token = access_token;
    credentials.refresh_token = refresh_token;
    credentials.expires_at = map.get("expires_at").and_then(|v| v.as_i64()).unwrap_or(0);
    Ok(credentials)
}

/// C++'s legacy `Credentials Load()` overload for the editor/harness code: an
/// empty session on read failure.
pub fn load_or_empty() -> Credentials {
    load().unwrap_or_default()
}

pub fn save(credentials: &Credentials) -> Result<(), String> {
    let normalized = normalize_console_url(&credentials.console_url)?;
    let directory = profile_directory();
    let path = credentials_path();
    if directory.is_empty() || path.is_empty() {
        return Err("no user profile directory is available".to_string());
    }
    ensure_secure_directory(&directory)?;
    if (!credentials.access_token.is_empty() && !session_token_is_safe(&credentials.access_token))
        || (!credentials.refresh_token.is_empty()
            && !session_token_is_safe(&credentials.refresh_token))
    {
        return Err("refusing to store an invalid cloud session token".to_string());
    }
    let document = serde_json::json!({
        "console_url": normalized,
        "email": credentials.email,
        "access_token": credentials.access_token,
        "refresh_token": credentials.refresh_token,
        "expires_at": credentials.expires_at,
    });
    let rendered = crate::io::json::dump_pretty(&document, 2) + "\n";
    write_document(&path, &rendered)
}

pub fn clear() -> Result<(), String> {
    let path = credentials_path();
    if path.is_empty() {
        return Ok(());
    }
    ensure_secure_directory(&profile_directory())?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("could not remove the local cloud session".to_string()),
    }
}
