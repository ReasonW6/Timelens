//! The light or dark look: the user's choice, or the app mode Windows asks for.
//! The choice is kept beside the data location pointer, outside the dataset.
use crate::i18n::tr;
use std::{
    fs,
    path::{Path, PathBuf},
};

use slint::ComponentHandle;
use windows::{
    Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    core::w,
};

use crate::{AppWindow, Theme, window_chrome};

pub const FILE: &str = "appearance.txt";

/// The choice as `Theme.mode` holds it: 0 follows the system, 1 light, 2 dark.
fn parse(text: &str) -> i32 {
    match text.trim() {
        "light" => 1,
        "dark" => 2,
        _ => 0,
    }
}

fn name(mode: i32) -> &'static str {
    match mode {
        1 => "light",
        2 => "dark",
        _ => "system",
    }
}

fn load(control: &Path) -> i32 {
    fs::read_to_string(control.join(FILE)).map_or(0, |text| parse(&text))
}

fn save(control: &Path, mode: i32) -> std::io::Result<()> {
    fs::create_dir_all(control)?;
    fs::write(control.join(FILE), name(mode))
}

/// Whether Windows asks apps to be dark; light when it cannot tell.
fn system_dark() -> bool {
    let mut value = 1_u32;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&raw mut size),
        )
    };
    status.is_ok() && value == 0
}

fn dark(mode: i32, system_dark: bool) -> bool {
    mode == 2 || (mode == 0 && system_dark)
}

fn show(window: &AppWindow, dark: bool) {
    let theme = window.global::<Theme>();
    if theme.get_dark() != dark {
        theme.set_dark(dark);
        window_chrome::apply(window.window(), dark);
    }
}

/// Applies the saved choice and saves new ones. The tray calls
/// [`follow_system`] when the system's settings change.
pub fn install(window: &AppWindow, control: PathBuf) {
    let theme = window.global::<Theme>();
    theme.set_mode(load(&control));
    show(window, dark(theme.get_mode(), system_dark()));

    let weak = window.as_weak();
    window.on_theme_mode_selected(move |mode| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        window.global::<Theme>().set_mode(mode);
        show(&window, dark(mode, system_dark()));
        if let Err(error) = save(&control, mode) {
            window.set_action_status(tr!("主题设置未能保存：{}", error).into());
        }
    });
}

/// Takes up the system's light or dark mode while the choice is to follow it.
pub fn follow_system(window: &AppWindow) {
    if window.global::<Theme>().get_mode() == 0 {
        show(window, system_dark());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_choice_round_trips_and_anything_else_follows_the_system() {
        let directory = tempfile::tempdir().unwrap();
        let control = directory.path().join("control");
        assert_eq!(load(&control), 0);
        for mode in [1, 2, 0] {
            save(&control, mode).unwrap();
            assert_eq!(load(&control), mode);
        }
        fs::write(control.join(FILE), "sepia").unwrap();
        assert_eq!(load(&control), 0);
    }

    #[test]
    fn only_following_the_system_reads_it() {
        assert!(dark(0, true));
        assert!(!dark(0, false));
        assert!(!dark(1, true));
        assert!(dark(2, false));
    }
}
