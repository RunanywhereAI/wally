//! Terminal capabilities: TTY detection, width, color policy.

/// `isatty` on a standard descriptor, the way the C++ asked: the CRT's
/// `_isatty` on Windows (so an MSYS/mintty pipe is *not* a terminal, unlike
/// `std::io::IsTerminal`), `isatty` elsewhere.
pub fn fd_is_tty(fd: i32) -> bool {
    #[cfg(windows)]
    {
        extern "C" {
            fn _isatty(fd: std::ffi::c_int) -> std::ffi::c_int;
        }
        // SAFETY: the CRT only inspects the descriptor.
        unsafe { _isatty(fd) != 0 }
    }
    #[cfg(not(windows))]
    {
        // SAFETY: isatty only inspects the descriptor.
        unsafe { libc::isatty(fd) == 1 }
    }
}

/// True when stdout is an interactive terminal.
pub fn stdout_is_tty() -> bool {
    fd_is_tty(1)
}

/// True when stderr is an interactive terminal.
pub fn stderr_is_tty() -> bool {
    fd_is_tty(2)
}

/// True when stdin is an interactive terminal (REPL gate).
pub fn stdin_is_tty() -> bool {
    fd_is_tty(0)
}

/// Columns of the controlling terminal, read from stderr (fallback 80).
pub fn terminal_width() -> i32 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_ERROR_HANDLE,
        };
        // SAFETY: plain Win32 query into a zeroed out-struct.
        unsafe {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(GetStdHandle(STD_ERROR_HANDLE), &mut info) != 0 {
                return i32::from(info.srWindow.Right - info.srWindow.Left + 1);
            }
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: TIOCGWINSZ fills a zeroed winsize; no other memory is touched.
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                return i32::from(ws.ws_col);
            }
        }
    }
    80
}

/// ANSI color allowed on stderr: TTY and NO_COLOR unset.
pub fn color_enabled() -> bool {
    stderr_is_tty() && std::env::var_os("NO_COLOR").is_none()
}
