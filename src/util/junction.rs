//! NTFS directory junctions: a folder that is another folder, which any user
//! can create (a symbolic link needs Developer Mode or an elevated token).

/// `IO_REPARSE_TAG_MOUNT_POINT`, the reparse tag of a junction.
const MOUNT_POINT_TAG: u32 = 0xA000_0003;

/// The `REPARSE_DATA_BUFFER` that makes a directory a junction to `target`,
/// an absolute path such as `C:\Users\me\.codex\.sandbox`. Kept free of
/// Windows APIs so its layout can be tested on any platform.
pub fn mount_point_buffer(target: &str) -> Vec<u8> {
    let target = target.strip_prefix(r"\\?\").unwrap_or(target);
    let substitute: Vec<u16> = format!(r"\??\{target}").encode_utf16().collect();
    let print: Vec<u16> = target.encode_utf16().collect();
    let substitute_bytes = (substitute.len() * 2) as u16;
    let print_bytes = (print.len() * 2) as u16;
    // The mount-point header after the generic one, then both names, each
    // NUL-terminated.
    let data_length = 8 + substitute_bytes + 2 + print_bytes + 2;
    let mut buffer = Vec::with_capacity(8 + usize::from(data_length));
    buffer.extend_from_slice(&MOUNT_POINT_TAG.to_le_bytes());
    buffer.extend_from_slice(&data_length.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes());
    buffer.extend_from_slice(&substitute_bytes.to_le_bytes());
    buffer.extend_from_slice(&(substitute_bytes + 2).to_le_bytes());
    buffer.extend_from_slice(&print_bytes.to_le_bytes());
    for unit in substitute.iter().chain(&[0]).chain(&print).chain(&[0]) {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    buffer
}

/// A path's text for comparing two spellings of one Windows path.
#[cfg(windows)]
fn comparable(path: &std::path::Path) -> String {
    let text = path.to_string_lossy();
    let text = text
        .strip_prefix(r"\\?\")
        .or_else(|| text.strip_prefix(r"\??\"))
        .unwrap_or(&text);
    text.trim_end_matches('\\').to_lowercase()
}

/// Whether `link` is already a junction (or symbolic link) to `target`.
#[cfg(windows)]
pub fn points_to(link: &std::path::Path, target: &std::path::Path) -> bool {
    std::fs::read_link(link).is_ok_and(|to| comparable(&to) == comparable(target))
}

/// Makes `link`, which must not exist, a junction to the directory `target`.
#[cfg(windows)]
pub fn create(link: &std::path::Path, target: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    /// `FSCTL_SET_REPARSE_POINT`.
    const SET_REPARSE_POINT: u32 = 0x0009_00A4;

    let target = std::path::absolute(target)?;
    let buffer = mount_point_buffer(&target.to_string_lossy());
    std::fs::create_dir(link)?;
    let wide: Vec<u16> = link.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call;
    // the remaining arguments are plain flags and null optional pointers.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = std::io::Error::last_os_error();
        let _ = std::fs::remove_dir(link);
        return Err(error);
    }
    let mut returned = 0u32;
    // SAFETY: `handle` is the directory just opened for writing; `buffer` is
    // a complete REPARSE_DATA_BUFFER of the length passed, read only.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    let error = std::io::Error::last_os_error();
    // SAFETY: `handle` came from CreateFileW above and is closed once.
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        let _ = std::fs::remove_dir(link);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(buffer: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([buffer[at], buffer[at + 1]])
    }

    fn name(buffer: &[u8], offset: u16, length: u16) -> String {
        let start = 16 + usize::from(offset);
        let units: Vec<u16> = buffer[start..start + usize::from(length)]
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn the_buffer_names_the_target_as_nt_path_and_as_printed() {
        let buffer = mount_point_buffer(r"C:\Users\o'brien\.codex\.sandbox");
        assert_eq!(&buffer[..4], &MOUNT_POINT_TAG.to_le_bytes());
        assert_eq!(usize::from(u16_at(&buffer, 4)), buffer.len() - 8);
        let (sub_at, sub_len) = (u16_at(&buffer, 8), u16_at(&buffer, 10));
        let (print_at, print_len) = (u16_at(&buffer, 12), u16_at(&buffer, 14));
        assert_eq!(sub_at, 0);
        assert_eq!(
            name(&buffer, sub_at, sub_len),
            r"\??\C:\Users\o'brien\.codex\.sandbox"
        );
        assert_eq!(print_at, sub_len + 2);
        assert_eq!(
            name(&buffer, print_at, print_len),
            r"C:\Users\o'brien\.codex\.sandbox"
        );
    }

    #[test]
    fn an_extended_length_target_loses_its_prefix() {
        let buffer = mount_point_buffer(r"\\?\C:\x");
        assert_eq!(name(&buffer, 0, u16_at(&buffer, 10)), r"\??\C:\x");
    }
}
