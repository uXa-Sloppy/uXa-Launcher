// Release builds use the Windows GUI subsystem so a normal launch has no
// console window. CLI commands attach to the parent console, or allocate one,
// and leave redirected stdout/stderr alone so `> file` still receives the text.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::Write;
use std::process::ExitCode;

#[cfg(windows)]
use std::fs::File;
#[cfg(windows)]
use std::os::windows::io::FromRawHandle;
#[cfg(windows)]
use std::sync::Mutex;

#[cfg(windows)]
use launcher_core::InheritedHandle;
use launcher_core::{usage_cli_result, write_cli_result};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        prepare_stdio();
    }
    match uxa_launcher_lib::parse_cli(&args) {
        // No flags opens the window. A single-instance check belongs only on
        // this path, so a CLI command still runs while the window is open.
        Ok(None) => {
            uxa_launcher_lib::run();
            ExitCode::SUCCESS
        }
        Ok(Some(command)) => {
            let outcome = uxa_launcher_lib::run_headless(command);
            emit(&outcome.stdout, false);
            emit(&outcome.stderr, true);
            ExitCode::from(outcome.code as u8)
        }
        Err(message) => {
            let command = args.join(" ");
            let record = usage_cli_result(env!("CARGO_PKG_VERSION"), &command, &message);
            let path = uxa_launcher_lib::cli_result_path();
            if let Err(err) = write_cli_result(&path, &record) {
                emit(
                    &format!("Could not write {}: {err}\n", path.display()),
                    true,
                );
            }
            let line = if message.ends_with('\n') {
                message
            } else {
                format!("{message}\n")
            };
            emit(&line, true);
            ExitCode::from(uxa_launcher_lib::EXIT_USAGE as u8)
        }
    }
}

fn emit(text: &str, error: bool) {
    if text.is_empty() {
        return;
    }
    let text = if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    };
    if write_bound(error, &text) || write_stdio(&text, error) {
        return;
    }
    #[cfg(windows)]
    write_console_fallback(&text);
}

fn write_stdio(text: &str, error: bool) -> bool {
    if error {
        let mut err = std::io::stderr();
        err.write_all(text.as_bytes())
            .and_then(|_| err.flush())
            .is_ok()
    } else {
        let mut out = std::io::stdout();
        out.write_all(text.as_bytes())
            .and_then(|_| out.flush())
            .is_ok()
    }
}

#[cfg(windows)]
struct BoundConsole {
    stdout: Option<File>,
    stderr: Option<File>,
}

#[cfg(windows)]
static BOUND: Mutex<BoundConsole> = Mutex::new(BoundConsole {
    stdout: None,
    stderr: None,
});

fn write_bound(error: bool, text: &str) -> bool {
    #[cfg(windows)]
    {
        let mut guard = match BOUND.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        let slot = if error {
            &mut guard.stderr
        } else {
            &mut guard.stdout
        };
        if let Some(file) = slot.as_mut() {
            return file
                .write_all(text.as_bytes())
                .and_then(|_| file.flush())
                .is_ok();
        }
        return false;
    }
    #[cfg(not(windows))]
    {
        let _ = (error, text);
        false
    }
}

/// Show CLI output in the shell that started a Windows GUI-subsystem exe.
///
/// A release build has no console of its own. `AttachConsole` does not point
/// stdout at that console, and opening `CONOUT$` with read+write often fails,
/// so earlier builds replaced a redirected pipe and then wrote nowhere.
/// Redirected handles are kept. Missing handles are bound to `CONOUT$`, and
/// the CLI text is written to that handle directly.
fn prepare_stdio() {
    #[cfg(windows)]
    {
        let plan = launcher_core::stdio_plan(
            inherited_handle(STD_OUTPUT_HANDLE),
            inherited_handle(STD_ERROR_HANDLE),
        );
        if !plan.attach_console {
            return;
        }
        unsafe {
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 && AllocConsole() == 0 {
                return;
            }
            if plan.replace_stdout {
                bind_standard_handle(STD_OUTPUT_HANDLE, true);
            }
            if plan.replace_stderr {
                bind_standard_handle(STD_ERROR_HANDLE, false);
            }
        }
    }
}

#[cfg(windows)]
fn inherited_handle(which: u32) -> InheritedHandle {
    const FILE_TYPE_CHAR: u32 = 0x0002;
    unsafe {
        let handle = GetStdHandle(which);
        if invalid_handle(handle) {
            return InheritedHandle::Missing;
        }
        match GetFileType(handle) & 0xFFFF {
            0 => InheritedHandle::Missing,
            FILE_TYPE_CHAR => InheritedHandle::Console,
            _ => InheritedHandle::Redirected,
        }
    }
}

#[cfg(windows)]
fn bind_standard_handle(which: u32, stdout: bool) {
    unsafe {
        let handle = open_conout();
        if invalid_handle(handle) {
            return;
        }
        let mut duplicated = std::ptr::null_mut();
        let process = GetCurrentProcess();
        if DuplicateHandle(
            process,
            handle,
            process,
            &mut duplicated,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        ) != 0
            && !invalid_handle(duplicated)
        {
            SetStdHandle(which, duplicated);
        }
        let file = File::from_raw_handle(handle as *mut std::ffi::c_void);
        if let Ok(mut guard) = BOUND.lock() {
            if stdout {
                guard.stdout = Some(file);
            } else {
                guard.stderr = Some(file);
            }
        }
    }
}

#[cfg(windows)]
fn open_conout() -> *mut std::ffi::c_void {
    let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
    unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    }
}

#[cfg(windows)]
fn write_console_fallback(text: &str) {
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if invalid_handle(handle) {
            return;
        }
        let bytes = text.as_bytes();
        let mut written = 0u32;
        if WriteFile(
            handle,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            std::ptr::null_mut(),
        ) != 0
        {
            return;
        }
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut written_chars = 0u32;
        WriteConsoleW(
            handle,
            wide.as_ptr(),
            wide.len() as u32,
            &mut written_chars,
            std::ptr::null_mut(),
        );
    }
}

#[cfg(windows)]
const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
#[cfg(windows)]
const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
#[cfg(windows)]
const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
#[cfg(windows)]
const GENERIC_WRITE: u32 = 0x4000_0000;
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x0000_0001;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
#[cfg(windows)]
const OPEN_EXISTING: u32 = 3;
#[cfg(windows)]
const DUPLICATE_SAME_ACCESS: u32 = 0x0000_0002;

#[cfg(windows)]
fn invalid_handle(handle: *mut std::ffi::c_void) -> bool {
    handle.is_null() || handle as isize == -1
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn AttachConsole(process_id: u32) -> i32;
    fn AllocConsole() -> i32;
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn GetStdHandle(std_handle: u32) -> *mut std::ffi::c_void;
    fn SetStdHandle(std_handle: u32, handle: *mut std::ffi::c_void) -> i32;
    fn GetFileType(handle: *mut std::ffi::c_void) -> u32;
    fn DuplicateHandle(
        source_process: *mut std::ffi::c_void,
        source: *mut std::ffi::c_void,
        target_process: *mut std::ffi::c_void,
        target: *mut *mut std::ffi::c_void,
        access: u32,
        inherit: i32,
        options: u32,
    ) -> i32;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut std::ffi::c_void,
        disposition: u32,
        flags: u32,
        template: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn WriteFile(
        handle: *mut std::ffi::c_void,
        buffer: *const u8,
        bytes: u32,
        written: *mut u32,
        overlapped: *mut std::ffi::c_void,
    ) -> i32;
    fn WriteConsoleW(
        handle: *mut std::ffi::c_void,
        buffer: *const u16,
        chars: u32,
        written: *mut u32,
        reserved: *mut std::ffi::c_void,
    ) -> i32;
}
