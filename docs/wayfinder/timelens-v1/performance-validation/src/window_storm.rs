use serde::Serialize;
use std::{env, fs, path::PathBuf, ptr::null_mut, thread, time::Instant};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, LWA_ALPHA, RegisterClassW, SW_HIDE,
        SW_MINIMIZE, SW_RESTORE, SW_SHOW, SetLayeredWindowAttributes, ShowWindow, WNDCLASSW,
        WS_EX_APPWINDOW, WS_EX_LAYERED, WS_OVERLAPPEDWINDOW,
    },
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Metric {
    cycles: usize,
    expected_state_operations: usize,
    elapsed_ms: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let mut output = None;
    let mut cycles = 2_500_usize;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => output = args.next().map(PathBuf::from),
            "--cycles" => cycles = args.next().ok_or("missing cycles")?.parse()?,
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    let output = output.ok_or("--out is required")?;
    let class = wide("Timelens.Performance.Storm");
    let title = wide("Timelens performance storm");
    let instance = unsafe { GetModuleHandleW(null_mut()) };
    let descriptor = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: class.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    unsafe { RegisterClassW(&descriptor) };

    let started = Instant::now();
    for _ in 0..cycles {
        let window = unsafe {
            CreateWindowExW(
                WS_EX_APPWINDOW | WS_EX_LAYERED,
                class.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                64,
                64,
                null_mut(),
                null_mut(),
                instance,
                null_mut(),
            )
        };
        if window.is_null() {
            return Err("CreateWindowExW failed".into());
        }
        unsafe {
            SetLayeredWindowAttributes(window, 0, 0, LWA_ALPHA);
            ShowWindow(window, SW_SHOW);
            ShowWindow(window, SW_MINIMIZE);
            ShowWindow(window, SW_RESTORE);
            ShowWindow(window, SW_HIDE);
            DestroyWindow(window);
        }
        if cycles > 1_000 && cycles % 250 == 0 {
            thread::yield_now();
        }
    }
    let metric = Metric {
        cycles,
        expected_state_operations: cycles * 5,
        elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0,
    };
    fs::write(output, serde_json::to_vec_pretty(&metric)?)?;
    println!("{}", serde_json::to_string_pretty(&metric)?);
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
