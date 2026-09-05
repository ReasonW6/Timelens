use std::{
    ffi::c_void,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::null_mut,
    sync::{Arc, Mutex, TryLockError},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use image::{ExtendedColorType, ImageEncoder, RgbaImage, imageops::FilterType};
use timelens_ipc::COLLECTOR_RESET_PAUSED_FILE;
use timelens_observer::{WindowObservation, WindowObserver};
use timelens_storage::{
    SnapshotDisplay, SnapshotMissingReason, SnapshotPolicy, SnapshotStoreRequest, SnapshotTrigger,
    Storage,
};
use windows::{
    Win32::{
        Foundation::{HANDLE, HMODULE, POINT, RECT},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
                ID3D11Texture2D,
            },
            Dxgi::Common::{
                DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_MODE_ROTATION,
                DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE90,
                DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
            },
            Dxgi::{
                CreateDXGIFactory1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
                DXGI_OUTPUT_DESC, IDXGIAdapter, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
                IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
            },
            Gdi::{
                BI_RGB, BITMAPINFO, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC,
                DeleteObject, HGDIOBJ, MONITOR_DEFAULTTONULL, MONITOR_DEFAULTTOPRIMARY,
                MonitorFromPoint, SelectObject,
            },
        },
        Storage::FileSystem::GetDiskFreeSpaceExW,
        System::{
            RemoteDesktop::{
                WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTSActive, WTSConnectState,
                WTSFreeMemory, WTSINFOEXW, WTSQuerySessionInformationW, WTSSessionInfoEx,
            },
            StationsAndDesktops::{
                CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS,
                DESKTOP_SWITCHDESKTOP, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
            },
            WindowsProgramming::QueryUnbiasedInterruptTimePrecise,
        },
        UI::WindowsAndMessaging::{
            CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetCursorPos,
            GetIconInfo, GetSystemMetrics, GetWindowRect, ICONINFO, SM_REMOTESESSION,
        },
    },
    core::{Interface, PCWSTR, PWSTR},
};

const LOW_DISK_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_SNAPSHOT_DIMENSION: u32 = 1280;
const SCHEDULER_POLL: Duration = Duration::from_millis(500);
static HEAVY_TASK_GATE: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CaptureSummary {
    pub stored: u32,
    pub missing: u32,
    pub encoded_bytes: u64,
    pub channel_range: Option<(u8, u8)>,
}

impl CaptureSummary {
    pub fn label(self) -> String {
        let diagnostics = self.channel_range.map_or_else(String::new, |(low, high)| {
            format!(" · WebP {} · 像素范围 {low}–{high}", self.encoded_bytes)
        });
        format!(
            "已保存 {} 张 · 缺失 {} 槽{diagnostics}",
            self.stored, self.missing
        )
    }
}

struct CapturedFrame {
    webp: Vec<u8>,
    width: u32,
    height: u32,
    channel_low: u8,
    channel_high: u8,
}

struct OutputTarget {
    adapter: IDXGIAdapter,
    output: IDXGIOutput,
    desc: DXGI_OUTPUT_DESC,
}

impl OutputTarget {
    fn display(&self) -> SnapshotDisplay {
        display_from_desc(&self.desc)
    }
}

pub fn spawn_scheduler(storage: Arc<Mutex<Storage>>, data_directory: PathBuf) {
    thread::spawn(move || {
        let mut scheduler = Scheduler::new(storage, data_directory);
        loop {
            if let Err(error) = scheduler.tick() {
                eprintln!("snapshot scheduler failed: {error:#}");
            }
            thread::sleep(SCHEDULER_POLL);
        }
    });
}

pub fn capture_now(storage: &Arc<Mutex<Storage>>, data_directory: &Path) -> Result<CaptureSummary> {
    run_heavy_task(|| {
        let policy = lock_storage(storage)?.snapshot_policy()?;
        let mut observer = WindowObserver::new().context("failed to initialize window observer")?;
        capture_slot(
            storage,
            data_directory,
            &policy,
            unix_time_ms(),
            SnapshotTrigger::Manual,
            &mut observer,
        )
    })
}

pub fn run_heavy_task<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = HEAVY_TASK_GATE
        .lock()
        .map_err(|_| anyhow!("heavy-task gate poisoned"))?;
    operation()
}

fn try_run_heavy_task<T>(operation: impl FnOnce() -> Result<T>) -> Option<Result<T>> {
    match HEAVY_TASK_GATE.try_lock() {
        Ok(_guard) => Some(operation()),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Poisoned(_)) => Some(Err(anyhow!("heavy-task gate poisoned"))),
    }
}

struct Scheduler {
    storage: Arc<Mutex<Storage>>,
    data_directory: PathBuf,
    observer: Option<WindowObserver>,
    schedule_key: Option<(i64, u32, bool)>,
    next_slot_utc_ms: i64,
    last_wall_utc_ms: i64,
    last_awake_100ns: u64,
}

impl Scheduler {
    fn new(storage: Arc<Mutex<Storage>>, data_directory: PathBuf) -> Self {
        Self {
            storage,
            data_directory,
            observer: None,
            schedule_key: None,
            next_slot_utc_ms: 0,
            last_wall_utc_ms: unix_time_ms(),
            last_awake_100ns: unbiased_interrupt_time_100ns(),
        }
    }

    fn tick(&mut self) -> Result<()> {
        let now = unix_time_ms();
        let policy = lock_storage(&self.storage)?.snapshot_policy()?;
        if !policy.enabled {
            self.schedule_key = None;
            self.last_wall_utc_ms = now;
            self.last_awake_100ns = unbiased_interrupt_time_100ns();
            return Ok(());
        }

        let key = (
            policy.cycle_started_utc_ms,
            policy.interval_minutes,
            policy.capture_all_displays,
        );
        let interval_ms = i64::from(policy.interval_minutes) * 60_000;
        if self.schedule_key != Some(key) {
            self.schedule_key = Some(key);
            self.next_slot_utc_ms = next_boundary(policy.cycle_started_utc_ms, interval_ms, now);
            self.last_wall_utc_ms = now;
            self.last_awake_100ns = unbiased_interrupt_time_100ns();
            return Ok(());
        }
        if now < self.next_slot_utc_ms {
            self.last_wall_utc_ms = now;
            self.last_awake_100ns = unbiased_interrupt_time_100ns();
            return Ok(());
        }

        let awake_now_100ns = unbiased_interrupt_time_100ns();
        let awake_elapsed = awake_now_100ns
            .saturating_sub(self.last_awake_100ns)
            .saturating_div(10_000)
            .min(i64::MAX as u64) as i64;
        let wall_elapsed = now.saturating_sub(self.last_wall_utc_ms);
        let resumed_from_sleep = sleep_detected(wall_elapsed, awake_elapsed);
        let latest_due = self.next_slot_utc_ms.saturating_add(
            now.saturating_sub(self.next_slot_utc_ms)
                .div_euclid(interval_ms)
                .saturating_mul(interval_ms),
        );

        while self.next_slot_utc_ms < latest_due {
            self.record_sleep_slot(self.next_slot_utc_ms, policy.capture_all_displays)?;
            self.next_slot_utc_ms = self.next_slot_utc_ms.saturating_add(interval_ms);
        }
        if resumed_from_sleep && latest_due < now.saturating_sub(5_000) {
            self.record_sleep_slot(latest_due, policy.capture_all_displays)?;
        } else {
            let captured = try_run_heavy_task(|| {
                if self.observer.is_none() {
                    self.observer = Some(
                        WindowObserver::new().context("failed to initialize window observer")?,
                    );
                }
                let observer = self.observer.as_mut().expect("observer is initialized");
                capture_slot(
                    &self.storage,
                    &self.data_directory,
                    &policy,
                    latest_due,
                    SnapshotTrigger::Scheduled,
                    observer,
                )?;
                Ok(())
            });
            match captured {
                Some(Ok(())) => {}
                Some(Err(error)) => {
                    eprintln!("scheduled snapshot slot failed: {error:#}");
                    record_session_missing(
                        &self.storage,
                        latest_due,
                        SnapshotTrigger::Scheduled,
                        SnapshotMissingReason::CaptureFailed,
                    )?;
                }
                None => {
                    eprintln!("scheduled snapshot slot skipped while another heavy task ran");
                    record_session_missing(
                        &self.storage,
                        latest_due,
                        SnapshotTrigger::Scheduled,
                        SnapshotMissingReason::CaptureFailed,
                    )?;
                }
            }
        }
        self.next_slot_utc_ms = latest_due.saturating_add(interval_ms);
        self.last_wall_utc_ms = now;
        self.last_awake_100ns = awake_now_100ns;
        Ok(())
    }

    fn record_sleep_slot(&self, slot_utc_ms: i64, all_displays: bool) -> Result<()> {
        let targets = if all_displays {
            enumerate_outputs().unwrap_or_default()
        } else {
            Vec::new()
        };
        let displays = if targets.is_empty() {
            vec![SnapshotDisplay::session()]
        } else {
            targets.iter().map(OutputTarget::display).collect()
        };
        record_missing(
            &self.storage,
            slot_utc_ms,
            SnapshotTrigger::Scheduled,
            &displays,
            SnapshotMissingReason::Sleep,
        )?;
        Ok(())
    }
}

fn capture_slot(
    storage: &Arc<Mutex<Storage>>,
    _data_directory: &Path,
    policy: &SnapshotPolicy,
    slot_utc_ms: i64,
    trigger: SnapshotTrigger,
    observer: &mut WindowObserver,
) -> Result<CaptureSummary> {
    let (data_directory, control_directory, paused) = {
        let s = lock_storage(storage)?;
        (
            s.data_directory().to_path_buf(),
            s.control_directory().to_path_buf(),
            s.collection_policy()?.paused,
        )
    };
    if control_directory.join(COLLECTOR_RESET_PAUSED_FILE).exists()
        || control_directory
            .join(timelens_ipc::COLLECTOR_RESET_REQUEST_FILE)
            .exists()
        || paused
    {
        return record_session_missing(
            storage,
            slot_utc_ms,
            trigger,
            SnapshotMissingReason::GlobalPause,
        );
    }
    if let Some(reason) = session_missing_reason() {
        return record_session_missing(storage, slot_utc_ms, trigger, reason);
    }
    if free_disk_bytes(&data_directory).is_some_and(|bytes| bytes < LOW_DISK_BYTES) {
        return record_session_missing(
            storage,
            slot_utc_ms,
            trigger,
            SnapshotMissingReason::LowDisk,
        );
    }

    let observations = observer
        .reconcile()
        .context("failed to reconcile windows for snapshot")?;
    if !observations.iter().any(|window| window.displayed) {
        return record_session_missing(
            storage,
            slot_utc_ms,
            trigger,
            SnapshotMissingReason::DesktopIdle,
        );
    }

    let outputs = enumerate_outputs().context("failed to enumerate desktop outputs")?;
    let targets = select_targets(outputs, &observations, policy.capture_all_displays);
    if targets.is_empty() {
        return record_session_missing(
            storage,
            slot_utc_ms,
            trigger,
            SnapshotMissingReason::CaptureFailed,
        );
    }
    let exclusions = lock_storage(storage)?.snapshot_exclusions()?;
    let captured_at = unix_time_ms().max(slot_utc_ms);
    let mut summary = CaptureSummary::default();

    for target in targets {
        let display = target.display();
        if display_has_excluded_window(&target.desc.DesktopCoordinates, &observations, &exclusions)
        {
            record_missing(
                storage,
                slot_utc_ms,
                trigger,
                std::slice::from_ref(&display),
                SnapshotMissingReason::PrivacyExclusion,
            )?;
            summary.missing += 1;
            continue;
        }
        match capture_output(&target) {
            Ok(frame) => {
                let request = SnapshotStoreRequest {
                    slot_started_utc_ms: slot_utc_ms,
                    captured_at_utc_ms: captured_at,
                    display,
                    pixel_width: frame.width,
                    pixel_height: frame.height,
                    trigger,
                    webp: frame.webp,
                };
                summary.encoded_bytes = summary
                    .encoded_bytes
                    .saturating_add(request.webp.len() as u64);
                summary.channel_range = Some(
                    summary
                        .channel_range
                        .map_or((frame.channel_low, frame.channel_high), |(low, high)| {
                            (low.min(frame.channel_low), high.max(frame.channel_high))
                        }),
                );
                lock_storage(storage)?.store_snapshot(&request)?;
                summary.stored += 1;
            }
            Err(error) => {
                eprintln!("snapshot capture failed for {}: {error:#}", display.key);
                record_missing(
                    storage,
                    slot_utc_ms,
                    trigger,
                    std::slice::from_ref(&display),
                    SnapshotMissingReason::CaptureFailed,
                )?;
                summary.missing += 1;
            }
        }
    }
    lock_storage(storage)?.apply_retention(unix_time_ms())?;
    Ok(summary)
}

fn record_session_missing(
    storage: &Arc<Mutex<Storage>>,
    slot_utc_ms: i64,
    trigger: SnapshotTrigger,
    reason: SnapshotMissingReason,
) -> Result<CaptureSummary> {
    record_missing(
        storage,
        slot_utc_ms,
        trigger,
        &[SnapshotDisplay::session()],
        reason,
    )?;
    Ok(CaptureSummary {
        stored: 0,
        missing: 1,
        ..CaptureSummary::default()
    })
}

fn record_missing(
    storage: &Arc<Mutex<Storage>>,
    slot_utc_ms: i64,
    trigger: SnapshotTrigger,
    displays: &[SnapshotDisplay],
    reason: SnapshotMissingReason,
) -> Result<u32> {
    let storage = lock_storage(storage)?;
    let mut inserted = 0;
    for display in displays {
        if storage
            .record_snapshot_missing(slot_utc_ms, display, trigger, reason)?
            .is_some()
        {
            inserted += 1;
        }
    }
    Ok(inserted)
}

fn enumerate_outputs() -> Result<Vec<OutputTarget>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut targets = Vec::new();
    for adapter_index in 0.. {
        let Ok(adapter1) = (unsafe { factory.EnumAdapters1(adapter_index) }) else {
            break;
        };
        let adapter: IDXGIAdapter = adapter1.cast()?;
        for output_index in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                break;
            };
            let desc = unsafe { output.GetDesc() }?;
            if desc.AttachedToDesktop.as_bool() {
                targets.push(OutputTarget {
                    adapter: adapter.clone(),
                    output,
                    desc,
                });
            }
        }
    }
    targets.sort_by_key(|target| {
        (
            target.desc.DesktopCoordinates.left,
            target.desc.DesktopCoordinates.top,
            target.display().key,
        )
    });
    Ok(targets)
}

fn select_targets(
    outputs: Vec<OutputTarget>,
    observations: &[WindowObservation],
    capture_all: bool,
) -> Vec<OutputTarget> {
    if capture_all || outputs.len() <= 1 {
        return outputs;
    }
    let focused_rect = observations
        .iter()
        .find(|window| window.focused)
        .and_then(window_rect);
    let display_rects = outputs
        .iter()
        .map(|output| output.desc.DesktopCoordinates)
        .collect::<Vec<_>>();
    let primary_monitor = unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) };
    let primary_index = outputs
        .iter()
        .position(|output| output.desc.Monitor == primary_monitor);
    let selected_index = select_display_index(
        &display_rects,
        focused_rect.as_ref(),
        cursor_position(),
        primary_index,
    );
    selected_index
        .and_then(|index| outputs.into_iter().nth(index))
        .into_iter()
        .collect()
}

fn display_has_excluded_window(
    display_rect: &RECT,
    observations: &[WindowObservation],
    exclusions: &[String],
) -> bool {
    observations.iter().any(|window| {
        window.displayed
            && exclusions
                .iter()
                .any(|identity| identity == &window.application_identity)
            && window_rect(window).is_some_and(|rect| intersection_area(&rect, display_rect) > 0)
    })
}

fn window_rect(window: &WindowObservation) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(
            windows::Win32::Foundation::HWND(window.window_id as *mut c_void),
            &mut rect,
        )
    }
    .ok()?;
    (rect.right > rect.left && rect.bottom > rect.top).then_some(rect)
}

fn cursor_position() -> Option<POINT> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok()?;
    let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONULL) };
    (!monitor.is_invalid()).then_some(point)
}

fn select_display_index(
    displays: &[RECT],
    focused: Option<&RECT>,
    cursor: Option<POINT>,
    primary_index: Option<usize>,
) -> Option<usize> {
    if let Some(focused) = focused {
        let mut selected = None;
        let mut largest_area = 0_i64;
        for (index, display) in displays.iter().enumerate() {
            let area = intersection_area(focused, display);
            if area > largest_area {
                selected = Some(index);
                largest_area = area;
            }
        }
        if selected.is_some() {
            return selected;
        }
    }
    if let Some(cursor) = cursor
        && let Some(index) = displays.iter().position(|display| {
            cursor.x >= display.left
                && cursor.x < display.right
                && cursor.y >= display.top
                && cursor.y < display.bottom
        })
    {
        return Some(index);
    }
    primary_index
        .filter(|index| *index < displays.len())
        .or_else(|| (!displays.is_empty()).then_some(0))
}

fn capture_output(target: &OutputTarget) -> Result<CapturedFrame> {
    let (device, context) = create_device(&target.adapter)?;
    let duplication = duplicate_output(&target.output, &device)?;
    let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource: Option<IDXGIResource> = None;
    let mut acquired = false;
    for _ in 0..3 {
        match unsafe { duplication.AcquireNextFrame(500, &mut frame_info, &mut resource) } {
            Ok(()) => {
                acquired = true;
                break;
            }
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
            Err(error) => return Err(error.into()),
        }
    }
    if !acquired {
        bail!("desktop duplication timed out");
    }
    let result = (|| {
        let texture: ID3D11Texture2D = resource
            .take()
            .context("desktop duplication returned no texture")?
            .cast()?;
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut source_desc) };
        let mut staging_desc = source_desc;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.BindFlags = 0;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        staging_desc.MiscFlags = 0;
        let mut staging = None;
        unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }?;
        let staging = staging.context("failed to create desktop staging texture")?;
        unsafe { context.CopyResource(&staging, &texture) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }?;
        let converted = copy_mapped_rgba(&source_desc, &mapped);
        unsafe { context.Unmap(&staging, 0) };
        let rgba = converted?;
        let mut image = RgbaImage::from_raw(source_desc.Width, source_desc.Height, rgba)
            .context("captured desktop buffer has inconsistent dimensions")?;
        image = rotate_image(image, target.desc.Rotation);
        overlay_cursor(&mut image, &target.desc.DesktopCoordinates)?;
        image = bound_image(image, MAX_SNAPSHOT_DIMENSION);
        let (width, height) = image.dimensions();
        let (channel_low, channel_high) = image
            .as_raw()
            .chunks_exact(4)
            .flat_map(|pixel| pixel[..3].iter().copied())
            .fold((u8::MAX, u8::MIN), |(low, high), channel| {
                (low.min(channel), high.max(channel))
            });
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp).write_image(
            image.as_raw(),
            width,
            height,
            ExtendedColorType::Rgba8,
        )?;
        Ok(CapturedFrame {
            webp,
            width,
            height,
            channel_low,
            channel_high,
        })
    })();
    let released = unsafe { duplication.ReleaseFrame() };
    result.and_then(|value| released.map(|_| value).map_err(Into::into))
}

fn create_device(adapter: &IDXGIAdapter) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }?;
    Ok((
        device.context("D3D11 did not return a device")?,
        context.context("D3D11 did not return a context")?,
    ))
}

fn duplicate_output(output: &IDXGIOutput, device: &ID3D11Device) -> Result<IDXGIOutputDuplication> {
    if let Ok(output5) = output.cast::<IDXGIOutput5>() {
        let formats = [DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM];
        if let Ok(duplication) = unsafe { output5.DuplicateOutput1(device, 0, &formats) } {
            return Ok(duplication);
        }
    }
    let output1: IDXGIOutput1 = output.cast()?;
    unsafe { output1.DuplicateOutput(device) }.map_err(Into::into)
}

fn copy_mapped_rgba(
    desc: &D3D11_TEXTURE2D_DESC,
    mapped: &D3D11_MAPPED_SUBRESOURCE,
) -> Result<Vec<u8>> {
    let bytes_per_pixel = if desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM {
        4
    } else if desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
        8
    } else {
        bail!("unsupported desktop duplication format {}", desc.Format.0);
    };
    let row_bytes = desc.Width as usize * bytes_per_pixel;
    if mapped.pData.is_null() || (mapped.RowPitch as usize) < row_bytes {
        bail!("desktop duplication returned an invalid mapped surface");
    }
    let pixel_count = desc.Width as usize * desc.Height as usize;
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for y in 0..desc.Height as usize {
        let row = unsafe {
            std::slice::from_raw_parts(
                (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                row_bytes,
            )
        };
        if desc.Format == DXGI_FORMAT_B8G8R8A8_UNORM {
            for pixel in row.chunks_exact(4) {
                rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
            }
        } else {
            for pixel in row.chunks_exact(8) {
                for channel in 0..3 {
                    let bits = u16::from_le_bytes([pixel[channel * 2], pixel[channel * 2 + 1]]);
                    rgba.push(tone_map_scrgb(half_to_f32(bits)));
                }
                rgba.push(255);
            }
        }
    }
    Ok(rgba)
}

fn rotate_image(image: RgbaImage, rotation: DXGI_MODE_ROTATION) -> RgbaImage {
    if rotation == DXGI_MODE_ROTATION_ROTATE90 {
        image::imageops::rotate90(&image)
    } else if rotation == DXGI_MODE_ROTATION_ROTATE180 {
        image::imageops::rotate180(&image)
    } else if rotation == DXGI_MODE_ROTATION_ROTATE270 {
        image::imageops::rotate270(&image)
    } else {
        debug_assert!(rotation == DXGI_MODE_ROTATION_IDENTITY || rotation == DXGI_MODE_ROTATION(0));
        image
    }
}

fn bound_image(image: RgbaImage, maximum: u32) -> RgbaImage {
    let (width, height) = image.dimensions();
    if width <= maximum && height <= maximum {
        return image;
    }
    let scale = maximum as f64 / f64::from(width.max(height));
    let new_width = (f64::from(width) * scale).round().max(1.0) as u32;
    let new_height = (f64::from(height) * scale).round().max(1.0) as u32;
    image::imageops::resize(&image, new_width, new_height, FilterType::Triangle)
}

fn overlay_cursor(image: &mut RgbaImage, desktop_rect: &RECT) -> Result<()> {
    let mut cursor = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetCursorInfo(&mut cursor) }?;
    if cursor.flags != CURSOR_SHOWING
        || cursor.ptScreenPos.x < desktop_rect.left
        || cursor.ptScreenPos.x >= desktop_rect.right
        || cursor.ptScreenPos.y < desktop_rect.top
        || cursor.ptScreenPos.y >= desktop_rect.bottom
    {
        return Ok(());
    }

    let width = image.width();
    let height = image.height();
    let mut info = BITMAPINFO::default();
    info.bmiHeader.biSize = std::mem::size_of_val(&info.bmiHeader) as u32;
    info.bmiHeader.biWidth = width as i32;
    info.bmiHeader.biHeight = -(height as i32);
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB.0;
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.is_invalid() {
        bail!("failed to create cursor composition context");
    }
    let mut bits = null_mut();
    let bitmap = match unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }
    {
        Ok(bitmap) => bitmap,
        Err(error) => {
            let _ = unsafe { DeleteDC(dc) };
            return Err(error.into());
        }
    };
    let old = unsafe { SelectObject(dc, HGDIOBJ(bitmap.0)) };
    let byte_count = width as usize * height as usize * 4;
    let dib = unsafe { std::slice::from_raw_parts_mut(bits as *mut u8, byte_count) };
    for (source, destination) in image.as_raw().chunks_exact(4).zip(dib.chunks_exact_mut(4)) {
        destination.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
    }
    let mut icon_info = ICONINFO::default();
    let draw_result =
        unsafe { GetIconInfo(cursor.hCursor.into(), &mut icon_info) }.and_then(|_| {
            let x = cursor.ptScreenPos.x - desktop_rect.left - icon_info.xHotspot as i32;
            let y = cursor.ptScreenPos.y - desktop_rect.top - icon_info.yHotspot as i32;
            unsafe { DrawIconEx(dc, x, y, cursor.hCursor.into(), 0, 0, 0, None, DI_NORMAL) }
        });
    if !icon_info.hbmMask.is_invalid() {
        let _ = unsafe { DeleteObject(HGDIOBJ(icon_info.hbmMask.0)) };
    }
    if !icon_info.hbmColor.is_invalid() {
        let _ = unsafe { DeleteObject(HGDIOBJ(icon_info.hbmColor.0)) };
    }
    if draw_result.is_ok() {
        for (source, destination) in dib.chunks_exact(4).zip(image.as_mut().chunks_exact_mut(4)) {
            destination.copy_from_slice(&[source[2], source[1], source[0], source[3]]);
        }
    }
    unsafe {
        SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(dc);
    }
    draw_result.map_err(Into::into)
}

fn session_missing_reason() -> Option<SnapshotMissingReason> {
    classify_session_state(
        session_is_disconnected(),
        session_is_locked(),
        unsafe { GetSystemMetrics(SM_REMOTESESSION) } != 0,
        input_desktop_name().as_deref(),
    )
}

fn classify_session_state(
    disconnected: Option<bool>,
    locked: Option<bool>,
    remote: bool,
    input_desktop_name: Option<&str>,
) -> Option<SnapshotMissingReason> {
    if disconnected == Some(true) {
        Some(SnapshotMissingReason::SessionDisconnected)
    } else if locked == Some(true) {
        Some(SnapshotMissingReason::Locked)
    } else if remote {
        Some(SnapshotMissingReason::RemoteSession)
    } else if !input_desktop_name.is_some_and(|name| name.eq_ignore_ascii_case("Default")) {
        Some(SnapshotMissingReason::SecureDesktop)
    } else {
        None
    }
}

fn session_is_disconnected() -> Option<bool> {
    let bytes = query_wts(WTSConnectState)?;
    if bytes.len() < std::mem::size_of::<i32>() {
        return None;
    }
    let state = i32::from_ne_bytes(bytes[..4].try_into().ok()?);
    Some(state != WTSActive.0)
}

fn session_is_locked() -> Option<bool> {
    let bytes = query_wts(WTSSessionInfoEx)?;
    if bytes.len() < std::mem::size_of::<WTSINFOEXW>() {
        return None;
    }
    let info = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<WTSINFOEXW>()) };
    if info.Level != 1 {
        return None;
    }
    let level = unsafe { info.Data.WTSInfoExLevel1 };
    Some(level.SessionFlags == WTS_SESSIONSTATE_LOCK as i32)
}

fn query_wts(class: windows::Win32::System::RemoteDesktop::WTS_INFO_CLASS) -> Option<Vec<u8>> {
    let mut buffer = PWSTR::null();
    let mut bytes = 0_u32;
    unsafe {
        WTSQuerySessionInformationW(None, WTS_CURRENT_SESSION, class, &mut buffer, &mut bytes)
    }
    .ok()?;
    if buffer.is_null() || bytes == 0 {
        return None;
    }
    let result =
        unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), bytes as usize) }
            .to_vec();
    unsafe { WTSFreeMemory(buffer.as_ptr().cast()) };
    Some(result)
}

fn input_desktop_name() -> Option<String> {
    let access = DESKTOP_ACCESS_FLAGS(DESKTOP_READOBJECTS.0 | DESKTOP_SWITCHDESKTOP.0);
    let desktop = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, access) }.ok()?;
    let mut required = 0_u32;
    let _ = unsafe {
        GetUserObjectInformationW(HANDLE(desktop.0), UOI_NAME, None, 0, Some(&mut required))
    };
    let mut buffer = vec![0_u16; (required as usize / 2).max(1)];
    let result = unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(buffer.as_mut_ptr().cast()),
            buffer.len() as u32 * 2,
            Some(&mut required),
        )
    };
    unsafe { CloseDesktop(desktop) }.ok()?;
    result.ok()?;
    let length = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..length]))
}

fn free_disk_bytes(directory: &Path) -> Option<u64> {
    let wide = directory
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut available = 0_u64;
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR::from_raw(wide.as_ptr()),
            Some(&mut available),
            None,
            None,
        )
    }
    .ok()?;
    Some(available)
}

fn display_from_desc(desc: &DXGI_OUTPUT_DESC) -> SnapshotDisplay {
    let name_length = desc
        .DeviceName
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(desc.DeviceName.len());
    let rect = desc.DesktopCoordinates;
    SnapshotDisplay {
        key: String::from_utf16_lossy(&desc.DeviceName[..name_length]),
        x: rect.left,
        y: rect.top,
        width: rect.right.saturating_sub(rect.left) as u32,
        height: rect.bottom.saturating_sub(rect.top) as u32,
        orientation_degrees: rotation_degrees(desc.Rotation),
    }
}

fn rotation_degrees(rotation: DXGI_MODE_ROTATION) -> u16 {
    if rotation == DXGI_MODE_ROTATION_ROTATE90 {
        90
    } else if rotation == DXGI_MODE_ROTATION_ROTATE180 {
        180
    } else if rotation == DXGI_MODE_ROTATION_ROTATE270 {
        270
    } else {
        0
    }
}

fn intersection_area(first: &RECT, second: &RECT) -> i64 {
    let width = first.right.min(second.right) - first.left.max(second.left);
    let height = first.bottom.min(second.bottom) - first.top.max(second.top);
    if width <= 0 || height <= 0 {
        0
    } else {
        i64::from(width) * i64::from(height)
    }
}

fn next_boundary(cycle_started_utc_ms: i64, interval_ms: i64, now_utc_ms: i64) -> i64 {
    if now_utc_ms < cycle_started_utc_ms {
        return cycle_started_utc_ms;
    }
    cycle_started_utc_ms.saturating_add(
        now_utc_ms
            .saturating_sub(cycle_started_utc_ms)
            .div_euclid(interval_ms)
            .saturating_add(1)
            .saturating_mul(interval_ms),
    )
}

fn unbiased_interrupt_time_100ns() -> u64 {
    unsafe { QueryUnbiasedInterruptTimePrecise() }
}

fn sleep_detected(wall_elapsed_ms: i64, awake_elapsed_ms: i64) -> bool {
    wall_elapsed_ms.saturating_sub(awake_elapsed_ms) > 5_000
}

fn half_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits & 0x8000) << 16;
    let exponent = (bits >> 10) & 0x1f;
    let mantissa = u32::from(bits & 0x03ff);
    let value = match exponent {
        0 if mantissa == 0 => sign,
        0 => {
            let mut mantissa = mantissa;
            let mut exponent = 113_u32;
            while mantissa & 0x0400 == 0 {
                mantissa <<= 1;
                exponent -= 1;
            }
            sign | (exponent << 23) | ((mantissa & 0x03ff) << 13)
        }
        31 => sign | 0x7f80_0000 | (mantissa << 13),
        _ => sign | ((u32::from(exponent) + 112) << 23) | (mantissa << 13),
    };
    f32::from_bits(value)
}

fn tone_map_scrgb(value: f32) -> u8 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    let mapped = value / (1.0 + value);
    let srgb = if mapped <= 0.003_130_8 {
        mapped * 12.92
    } else {
        1.055 * mapped.powf(1.0 / 2.4) - 0.055
    };
    (srgb.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn lock_storage(storage: &Arc<Mutex<Storage>>) -> Result<std::sync::MutexGuard<'_, Storage>> {
    storage.lock().map_err(|_| anyhow!("storage lock poisoned"))
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_slots_start_at_the_next_boundary() {
        assert_eq!(next_boundary(1_000, 300_000, 1_000), 301_000);
        assert_eq!(next_boundary(1_000, 300_000, 300_999), 301_000);
        assert_eq!(next_boundary(1_000, 300_000, 301_000), 601_000);
    }

    #[test]
    fn sleep_detection_uses_unbiased_awake_time() {
        assert!(!sleep_detected(500, 490));
        assert!(!sleep_detected(5_500, 500));
        assert!(sleep_detected(5_501, 500));
        assert!(sleep_detected(60_000, 500));
    }

    #[test]
    fn heavy_task_gate_prevents_scheduled_overlap() {
        let guard = HEAVY_TASK_GATE.lock().unwrap();
        assert!(try_run_heavy_task(|| Ok(7_u8)).is_none());
        drop(guard);
        assert_eq!(run_heavy_task(|| Ok(7_u8)).unwrap(), 7);
    }

    #[test]
    fn intersection_uses_positive_area_only() {
        let first = RECT {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        };
        let overlapping = RECT {
            left: 50,
            top: 25,
            right: 150,
            bottom: 75,
        };
        let touching = RECT {
            left: 100,
            top: 0,
            right: 200,
            bottom: 100,
        };
        assert_eq!(intersection_area(&first, &overlapping), 2_500);
        assert_eq!(intersection_area(&first, &touching), 0);
    }

    #[test]
    fn active_display_prefers_focused_area_then_cursor_then_primary() {
        let displays = [
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            RECT {
                left: 1920,
                top: 0,
                right: 3840,
                bottom: 1080,
            },
        ];
        let crossing_focus = RECT {
            left: 1800,
            top: 100,
            right: 2600,
            bottom: 900,
        };
        assert_eq!(
            select_display_index(
                &displays,
                Some(&crossing_focus),
                Some(POINT { x: 100, y: 100 }),
                Some(0),
            ),
            Some(1)
        );
        assert_eq!(
            select_display_index(&displays, None, Some(POINT { x: 2500, y: 500 }), Some(0),),
            Some(1)
        );
        assert_eq!(
            select_display_index(&displays, None, None, Some(0)),
            Some(0)
        );
        assert_eq!(select_display_index(&[], None, None, None), None);
    }

    #[test]
    fn session_boundaries_are_conservative_and_have_stable_precedence() {
        assert_eq!(
            classify_session_state(Some(true), Some(true), true, Some("Winlogon")),
            Some(SnapshotMissingReason::SessionDisconnected)
        );
        assert_eq!(
            classify_session_state(Some(false), Some(true), true, Some("Winlogon")),
            Some(SnapshotMissingReason::Locked)
        );
        assert_eq!(
            classify_session_state(Some(false), Some(false), true, Some("Default")),
            Some(SnapshotMissingReason::RemoteSession)
        );
        assert_eq!(
            classify_session_state(Some(false), Some(false), false, Some("Winlogon")),
            Some(SnapshotMissingReason::SecureDesktop)
        );
        assert_eq!(
            classify_session_state(Some(false), Some(false), false, None),
            Some(SnapshotMissingReason::SecureDesktop)
        );
        assert_eq!(
            classify_session_state(Some(false), Some(false), false, Some("default")),
            None
        );
    }

    #[test]
    fn half_float_and_tone_mapping_cover_sdr_and_hdr_values() {
        assert_eq!(half_to_f32(0x0000), 0.0);
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0x4000), 2.0);
        assert_eq!(tone_map_scrgb(-1.0), 0);
        assert!(tone_map_scrgb(2.0) > tone_map_scrgb(1.0));
    }
}
