use crate::i18n::tr;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use windows::{
    Win32::{
        Foundation::VARIANT_BOOL,
        System::{
            Com::{
                COINIT, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoInitializeEx,
                CoUninitialize,
            },
            TaskScheduler::{
                IExecAction, IRegisteredTask, TASK_RUNLEVEL_HIGHEST, TASK_RUNLEVEL_TYPE,
                TASK_TRIGGER_LOGON, TASK_TRIGGER_TYPE2,
            },
        },
    },
    core::{BSTR, Interface},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_CANCELLED, WAIT_OBJECT_0},
    System::{
        SystemInformation::GetSystemDirectoryW,
        Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject},
    },
    UI::{
        Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
        WindowsAndMessaging::SW_HIDE,
    },
};

#[derive(Debug, PartialEq, Eq)]
pub struct StartupState {
    pub enabled: bool,
    pub consistent: bool,
}

struct Installation {
    task_folder: String,
    core_directory: PathBuf,
    collector_directory: PathBuf,
}

struct ComApartment;
impl ComApartment {
    fn new(model: COINIT) -> Result<Self> {
        unsafe { CoInitializeEx(None, model) }.ok()?;
        Ok(Self)
    }
}
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn executable(task: &IRegisteredTask) -> Result<PathBuf> {
    unsafe {
        let actions = task.Definition()?.Actions()?;
        let mut count = 0;
        actions.Count(&mut count)?;
        if count != 1 {
            bail!(tr!("Timelens 启动任务的程序配置不正确"));
        }
        let mut path = BSTR::new();
        actions
            .get_Item(1)?
            .cast::<IExecAction>()?
            .Path(&mut path)?;
        Ok(PathBuf::from(path.to_string()))
    }
}

fn logon_enabled(task: &IRegisteredTask) -> Result<bool> {
    unsafe {
        let triggers = task.Definition()?.Triggers()?;
        let mut count = 0;
        triggers.Count(&mut count)?;
        let mut found = false;
        let mut enabled = false;
        for index in 1..=count {
            let trigger = triggers.get_Item(index)?;
            let mut kind = TASK_TRIGGER_TYPE2::default();
            trigger.Type(&mut kind)?;
            if kind == TASK_TRIGGER_LOGON {
                found = true;
                let mut state = VARIANT_BOOL::default();
                trigger.Enabled(&mut state)?;
                enabled |= state.as_bool();
            }
        }
        if !found {
            bail!(tr!("Timelens 启动任务缺少登录触发器，请重新安装"));
        }
        Ok(task.Enabled()?.as_bool() && enabled)
    }
}

fn installed_state(collector_task: &str) -> Result<(StartupState, Installation)> {
    if !crate::collector_task::valid_task_path(collector_task) {
        bail!(tr!("Timelens 启动任务路径不正确"));
    }
    let _com = ComApartment::new(COINIT_MULTITHREADED)?;
    let (folder, _) = collector_task
        .rsplit_once('\\')
        .context(tr!("启动任务路径不正确"))?;
    let core = crate::collector_task::open_task(&format!(r"{folder}\Core"))
        .context(tr!("未找到 Timelens 启动任务，请使用安装版"))?;
    let collector = crate::collector_task::open_task(collector_task)
        .context(tr!("未找到 Timelens 采集器启动任务，请重新安装"))?;
    let mut collector_level = TASK_RUNLEVEL_TYPE::default();
    unsafe {
        collector
            .Definition()?
            .Principal()?
            .RunLevel(&mut collector_level)?
    };
    if collector_level != TASK_RUNLEVEL_HIGHEST {
        bail!(tr!("Timelens 采集器启动任务的权限配置不正确，请重新安装"));
    }
    let current = std::env::current_exe()?;
    if !executable(&core)?
        .to_string_lossy()
        .eq_ignore_ascii_case(&current.to_string_lossy())
    {
        bail!(tr!(
            "当前程序与已安装的 Timelens 不同，请在安装版中设置开机自启动"
        ));
    }
    let collector_exe = executable(&collector)?;
    if !collector_exe.file_name().is_some_and(|name| {
        name.to_string_lossy()
            .eq_ignore_ascii_case("Timelens.Collector.exe")
    }) {
        bail!(tr!("Timelens 采集器启动任务的程序配置不正确"));
    }
    let core_enabled = logon_enabled(&core)?;
    let collector_enabled = logon_enabled(&collector)?;
    Ok((
        StartupState {
            enabled: core_enabled && collector_enabled,
            consistent: core_enabled == collector_enabled,
        },
        Installation {
            task_folder: format!("{folder}\\"),
            core_directory: current
                .parent()
                .context(tr!("安装目录不可用"))?
                .to_path_buf(),
            collector_directory: collector_exe
                .parent()
                .context(tr!("采集器目录不可用"))?
                .to_path_buf(),
        },
    ))
}

pub fn read(collector_task: &str) -> Result<StartupState> {
    installed_state(collector_task).map(|(state, _)| state)
}

// These are arguments to powershell.exe -File, never an interpolated command.
fn quote_argument(value: &str) -> Result<String> {
    if value.contains(['"', '\0', '\r', '\n']) {
        bail!(tr!("启动设置包含无效的路径字符"));
    }
    let trailing = value.chars().rev().take_while(|c| *c == '\\').count();
    Ok(format!("\"{value}{}\"", "\\".repeat(trailing)))
}

pub fn set(collector_task: &str, enabled: bool) -> Result<()> {
    let (_, installation) = installed_state(collector_task)?;
    let script = installation
        .collector_directory
        .join(r"internal\maintenance.ps1");
    if !script.is_file() {
        bail!(tr!("启动设置组件缺失，请重新安装 Timelens"));
    }
    let mut system = vec![0_u16; 32768];
    let count = unsafe { GetSystemDirectoryW(system.as_mut_ptr(), system.len() as u32) } as usize;
    if count == 0 || count >= system.len() {
        bail!(tr!("无法定位 Windows 系统目录"));
    }
    let powershell = PathBuf::from(String::from_utf16(&system[..count])?)
        .join(r"WindowsPowerShell\v1.0\powershell.exe");
    let parameters = format!(
        "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File {} -Mode SetStartup -InstallDir {} -ElevatedDir {} -TaskPath {} -StartupState {}",
        quote_argument(&script.to_string_lossy())?,
        quote_argument(&installation.core_directory.to_string_lossy())?,
        quote_argument(&installation.collector_directory.to_string_lossy())?,
        quote_argument(&installation.task_folder)?,
        if enabled { "Enabled" } else { "Disabled" },
    );
    let wide = |value: &str| value.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let file = wide(&powershell.to_string_lossy());
    let parameters = wide(&parameters);
    let verb = wide("runas");
    let directory = wide(&installation.collector_directory.to_string_lossy());
    let _com = ComApartment::new(COINIT_APARTMENTTHREADED)?;
    let mut execute = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        lpDirectory: directory.as_ptr(),
        nShow: SW_HIDE,
        ..Default::default()
    };
    if unsafe { ShellExecuteExW(&mut execute) } == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            bail!(tr!("已取消管理员确认，开机自启动保持原设置"));
        }
        return Err(error).context(tr!("无法打开启动设置的管理员确认"));
    }
    if execute.hProcess.is_null() {
        bail!(tr!("无法确认启动设置是否完成，请重新打开常规查看状态"));
    }
    let waited = unsafe { WaitForSingleObject(execute.hProcess, INFINITE) };
    let mut code = 0;
    let exited = unsafe { GetExitCodeProcess(execute.hProcess, &mut code) };
    unsafe { CloseHandle(execute.hProcess) };
    if waited != WAIT_OBJECT_0 || exited == 0 || code != 0 {
        bail!(tr!(
            "更改开机自启动失败（{}），请确认 Timelens 安装完整后重试",
            code
        ));
    }
    let saved = read(collector_task)?;
    if !saved.consistent || saved.enabled != enabled {
        bail!(tr!("启动任务状态与所选设置不一致，请重试"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::{Foundation::LocalFree, UI::Shell::CommandLineToArgvW};

    #[test]
    fn file_arguments_round_trip_paths_and_preserve_literal_shell_characters() {
        let values = [
            r"C:\程序 文件\Timelens\internal\maintenance.ps1",
            r"\Timelens\",
            r"D:\$data`dir;\",
        ];
        let command = format!(
            "powershell.exe {}",
            values
                .iter()
                .map(|v| quote_argument(v).unwrap())
                .collect::<Vec<_>>()
                .join(" ")
        );
        let command: Vec<_> = command.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let mut count = 0;
            let args = CommandLineToArgvW(command.as_ptr(), &mut count);
            assert!(!args.is_null());
            assert_eq!(count as usize, values.len() + 1);
            for (index, expected) in values.iter().enumerate() {
                let pointer = *args.add(index + 1);
                let mut length = 0;
                while *pointer.add(length) != 0 {
                    length += 1;
                }
                assert_eq!(
                    String::from_utf16(std::slice::from_raw_parts(pointer, length)).unwrap(),
                    *expected
                );
            }
            LocalFree(args.cast());
        }
        assert!(quote_argument("bad\"path").is_err());
        assert!(quote_argument("bad\npath").is_err());
    }
}
