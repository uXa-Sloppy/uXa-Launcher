// Release builds use the Windows GUI subsystem so a normal launch has no
// console window. CLI commands attach to the parent console instead.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match uxa_launcher_lib::parse_cli(&args) {
        // No flags opens the window. A single-instance check belongs only on
        // this path, so a CLI command still runs while the window is open.
        Ok(None) => {
            uxa_launcher_lib::run();
            ExitCode::SUCCESS
        }
        Ok(Some(command)) => {
            attach_parent_console();
            ExitCode::from(uxa_launcher_lib::run_headless(command) as u8)
        }
        Err(message) => {
            attach_parent_console();
            eprintln!("{message}");
            ExitCode::from(uxa_launcher_lib::EXIT_USAGE as u8)
        }
    }
}

/// Show CLI output in the shell that started a Windows GUI-subsystem exe.
#[cfg(windows)]
fn attach_parent_console() {
    use std::ffi::c_void;

    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;

    #[link(name = "kernel32")]
    extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
        fn CreateFileA(
            name: *const u8,
            access: u32,
            share: u32,
            security: *mut c_void,
            disposition: u32,
            flags: u32,
            template: *mut c_void,
        ) -> *mut c_void;
        fn SetStdHandle(std_handle: u32, handle: *mut c_void) -> i32;
    }

    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let name = b"CONOUT$\0";
        let access = GENERIC_READ | GENERIC_WRITE;
        let share = FILE_SHARE_READ | FILE_SHARE_WRITE;
        let stdout = CreateFileA(
            name.as_ptr(),
            access,
            share,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        let stderr = CreateFileA(
            name.as_ptr(),
            access,
            share,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if !invalid_handle(stdout) {
            SetStdHandle(STD_OUTPUT_HANDLE, stdout);
        }
        if !invalid_handle(stderr) {
            SetStdHandle(STD_ERROR_HANDLE, stderr);
        }
    }
}

#[cfg(windows)]
fn invalid_handle(handle: *mut std::ffi::c_void) -> bool {
    handle.is_null() || handle as isize == -1
}

#[cfg(not(windows))]
fn attach_parent_console() {}
