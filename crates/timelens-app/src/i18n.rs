//! The interface language: the user's choice, or the Windows display language.
//! The choice is kept beside the data location pointer, outside the dataset, so
//! the recovery window speaks it too.
//!
//! The source text is Simplified Chinese. Slint bundles the translations of the
//! `@tr` strings in `.slint` files; text written in Rust goes through [`tr!`]
//! and [`trn!`], which look it up in `lang/<language>/LC_MESSAGES/messages.po`.
use std::{
    borrow::Cow,
    fmt::Display,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU8, Ordering},
};

use slint::ComponentHandle;

use crate::{AppWindow, Theme};

pub const FILE: &str = "language.txt";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Language {
    ZhCn,
    ZhTw,
    En,
    Ja,
    Ko,
}

/// The General page's choices in order; the first follows Windows.
const CHOICES: [&str; 6] = ["system", "zh-CN", "zh-TW", "en", "ja", "ko"];

impl Language {
    const ALL: [Self; 5] = [Self::ZhCn, Self::ZhTw, Self::En, Self::Ja, Self::Ko];

    /// The name Slint's bundled translations go by; the source has none.
    fn bundle(self) -> &'static str {
        match self {
            Self::ZhCn => "",
            other => CHOICES[other as usize + 1],
        }
    }

    /// A font every Windows 11 installation has for the language. Traditional
    /// Chinese keeps YaHei: JhengHei is an optional feature.
    fn font(self) -> &'static str {
        match self {
            Self::ZhCn | Self::ZhTw => "Microsoft YaHei UI",
            Self::En => "Segoe UI",
            Self::Ja => "Yu Gothic UI",
            Self::Ko => "Malgun Gothic",
        }
    }

    /// The column in the generated catalogs; the source has none.
    fn column(self) -> Option<usize> {
        (self as usize).checked_sub(1)
    }

    /// Which plural form a count takes: only English has two.
    fn plural(self, count: u64) -> usize {
        usize::from(self == Self::En && count != 1)
    }
}

/// A Windows language name such as `zh-HK` or `ja-JP` as an interface language;
/// anything else reads as English.
pub fn from_tag(tag: &str) -> Language {
    let tag = tag.to_ascii_lowercase().replace('_', "-");
    let parts = tag.split('-').collect::<Vec<_>>();
    match parts[0] {
        "zh" if parts.contains(&"hans") => Language::ZhCn,
        "zh" if parts
            .iter()
            .any(|part| matches!(*part, "hant" | "tw" | "hk" | "mo")) =>
        {
            Language::ZhTw
        }
        "zh" => Language::ZhCn,
        "ja" => Language::Ja,
        "ko" => Language::Ko,
        _ => Language::En,
    }
}

/// The first of the user's Windows display languages, such as `zh-TW`.
fn system_tag() -> Option<String> {
    use windows_sys::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
    let (mut count, mut length) = (0_u32, 0_u32);
    let sized = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            std::ptr::null_mut(),
            &mut length,
        )
    };
    if sized == 0 || length == 0 {
        return None;
    }
    let mut names = vec![0_u16; length as usize];
    let read = unsafe {
        GetUserPreferredUILanguages(
            MUI_LANGUAGE_NAME,
            &mut count,
            names.as_mut_ptr(),
            &mut length,
        )
    };
    let first = names.split(|unit| *unit == 0).next()?;
    (read != 0 && !first.is_empty()).then(|| String::from_utf16_lossy(first))
}

/// The language a choice stands for: following Windows reads its display
/// language.
fn resolve(choice: i32, system: impl FnOnce() -> Option<String>) -> Language {
    match choice {
        1..=5 => Language::ALL[choice as usize - 1],
        _ => system().map_or(Language::En, |tag| from_tag(&tag)),
    }
}

fn parse(text: &str) -> i32 {
    CHOICES
        .iter()
        .position(|choice| *choice == text.trim())
        .map_or(0, |index| index as i32)
}

fn load(control: &Path) -> i32 {
    fs::read_to_string(control.join(FILE)).map_or(0, |text| parse(&text))
}

fn save(control: &Path, choice: i32) -> std::io::Result<()> {
    fs::create_dir_all(control)?;
    fs::write(control.join(FILE), CHOICES[choice.clamp(0, 5) as usize])
}

/// The source language until [`init`] reads the choice, which keeps tests on
/// the text they were written against.
static CURRENT: AtomicU8 = AtomicU8::new(Language::ZhCn as u8);

pub fn current() -> Language {
    Language::ALL[usize::from(CURRENT.load(Ordering::Relaxed))]
}

fn set(language: Language) {
    CURRENT.store(language as u8, Ordering::Relaxed);
}

/// Takes up the saved choice before any text is written.
pub fn init(control: &Path) {
    set(resolve(load(control), system_tag));
}

/// Points Slint's `@tr` and the window's font at the current language. Slint
/// keeps its translations with the first component, so one must exist.
pub fn apply_interface(theme: &Theme) {
    if let Err(error) = slint::select_bundled_translation(current().bundle()) {
        eprintln!("interface language unavailable: {error}");
    }
    theme.set_font_ui(current().font().into());
}

/// Shows the saved choice on the General page and switches when it changes;
/// `relabel` rewrites what Rust wrote in the previous language.
pub fn install(window: &AppWindow, control: PathBuf, relabel: impl Fn(&AppWindow) + 'static) {
    window.set_language_choice(load(&control));
    apply_interface(&window.global::<Theme>());
    let weak = window.as_weak();
    window.on_language_selected(move |choice| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        window.set_language_choice(choice);
        set(resolve(choice, system_tag));
        apply_interface(&window.global::<Theme>());
        relabel(&window);
        if let Err(error) = save(&control, choice) {
            window.set_action_status(tr!("语言设置未能保存：{}", error).into());
        }
    });
}

/// Each source text with its forms in Traditional Chinese, English, Japanese
/// and Korean; one form per plural form, none where untranslated.
type Entry = (&'static str, [&'static [&'static str]; 4]);

static MESSAGES: &[Entry] = include!(concat!(env!("OUT_DIR"), "/messages.rs"));

fn lookup(
    catalog: &'static [Entry],
    language: Language,
    source: &'static str,
    count: u64,
) -> &'static str {
    let Some(column) = language.column() else {
        return source;
    };
    catalog
        .binary_search_by(|(key, _)| (*key).cmp(source))
        .ok()
        .and_then(|index| {
            let forms = catalog[index].1[column];
            forms.get(language.plural(count)).or(forms.first()).copied()
        })
        .unwrap_or(source)
}

/// A source text in the current language.
pub fn text(source: &'static str) -> &'static str {
    lookup(MESSAGES, current(), source, 1)
}

/// A source text in the current language, in the form that suits `count`.
pub fn plural(source: &'static str, count: u64) -> &'static str {
    lookup(MESSAGES, current(), source, count)
}

/// Puts the arguments in a translated text: `{}` takes the next one, `{0}`
/// names one, so a translation can reorder them.
pub fn fill(template: &str, args: &[&dyn Display]) -> String {
    let mut output = String::with_capacity(template.len() + 16);
    let mut next = 0;
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        output.push_str(&rest[..open]);
        rest = &rest[open..];
        let argument = rest.find('}').and_then(|close| {
            let name = &rest[1..close];
            let index = if name.is_empty() {
                next += 1;
                next - 1
            } else {
                name.parse().ok()?
            };
            Some((close, args.get(index)?))
        });
        match argument {
            Some((close, argument)) => {
                output.push_str(&localize(&argument.to_string()));
                rest = &rest[close + 1..];
            }
            None => {
                output.push('{');
                rest = &rest[1..];
            }
        }
    }
    output.push_str(rest);
    output
}

/// A text in the current language, with arguments filled in.
macro_rules! tr {
    ($source:literal) => {
        $crate::i18n::text($source)
    };
    ($source:literal, $($argument:expr),+ $(,)?) => {
        $crate::i18n::fill(
            $crate::i18n::text($source),
            &[$(&$argument as &dyn ::std::fmt::Display),+],
        )
    };
}
pub(crate) use tr;

/// Like [`tr!`], in the plural form that suits the count.
macro_rules! trn {
    ($count:expr, $source:literal $(, $argument:expr)* $(,)?) => {
        $crate::i18n::fill(
            $crate::i18n::plural($source, $count as u64),
            &[$(&$argument as &dyn ::std::fmt::Display),*],
        )
    };
}
pub(crate) use trn;

/// Failures storage describes in Simplified Chinese. They reach the interface
/// inside its errors, and read in the current language there.
const STORAGE_DETAILS: [&str; 46] = [
    "请选择两个不同应用",
    "恢复目录必须与原数据目录分开",
    "请选择一个新的空恢复目录",
    "恢复目录无效",
    "无法读取原数据库的数据结构",
    "恢复引用包含未知表",
    "无法定位损坏的引用记录",
    "恢复引用无法修复",
    "快照记录无效",
    "快照哈希无效",
    "快照像素无法校验",
    "请选择本地固定磁盘的绝对路径",
    "目标父目录不可用",
    "数据位置不能经过符号链接、联接或云占位目录",
    "数据位置必须是目录",
    "不支持网络数据目录",
    "磁盘根路径无效",
    "数据目录必须位于本地固定磁盘",
    "数据目录不能放在云同步目录",
    "目标位于已注册的云同步根目录",
    "数据路径无法规范化",
    "新旧数据目录不能相同或互相包含",
    "目标目录必须为空；V1 不合并数据集",
    "无法在这个位置创建目录。请选择有写入权限的位置，或先建好一个空目录再迁移",
    "数据集快照路径无效",
    "切换未完成，原数据仍使用；已验证的副本位于",
    "数据库引用完整性检查失败",
    "数据库完整性检查失败",
    "备份数据库版本无效",
    "备份包含未知或被修改的数据库结构",
    "检测到数据损坏，已停止写入。请进入恢复界面，保留原件后恢复到新目录。",
    "独立密码不能为空或超过 1024 字节",
    "目标文件已存在，请选择新文件名",
    "备份路径无效",
    "ZIP 文件数量超出限制",
    "ZIP 包含重复、越界或未知路径",
    "ZIP 容量溢出",
    "ZIP 解压大小超出限制",
    "备份清单无效或密码错误",
    "不兼容的备份格式版本",
    "清单与 ZIP 内容不一致",
    "数据库版本与备份清单不一致",
    "图片数量与数据库不一致",
    "快照像素大小超出限制",
    "快照像素校验失败",
    "图片哈希无效",
];

/// A storage error as it reads in the current language.
pub fn localize(text: &str) -> Cow<'_, str> {
    let cjk = |c: char| ('\u{4e00}'..='\u{9fff}').contains(&c);
    if current() == Language::ZhCn || !text.chars().any(cjk) {
        return Cow::Borrowed(text);
    }
    let mut text = text.to_owned();
    for detail in STORAGE_DETAILS {
        if text.contains(detail) {
            text = text.replace(detail, self::text(detail));
        }
    }
    Cow::Owned(text)
}

/// Words that mark a failure in each language. A status that has one stays in
/// the toast until dismissed, whichever language it was written in.
const FAILURE_WORDS: [&[&str]; 5] = [
    &["失败", "错误", "无法", "未能", "未完成", "请输入"],
    &["失敗", "錯誤", "無法", "未能", "未完成", "請輸入"],
    &[
        "fail",
        "error",
        "cannot",
        "could not",
        "unable",
        "incomplete",
        "enter a valid",
    ],
    &[
        "失敗",
        "エラー",
        "できません",
        "できなかった",
        "未完了",
        "入力してください",
    ],
    &["실패", "오류", "수 없", "못했", "완료되지", "입력하세요"],
];

pub fn reads_as_failure(message: &str) -> bool {
    let lower = message.to_lowercase();
    FAILURE_WORDS
        .iter()
        .flat_map(|words| words.iter())
        .any(|word| lower.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    static INTERFACE: &[Entry] = include!(concat!(env!("OUT_DIR"), "/interface.rs"));

    #[test]
    fn windows_language_names_pick_the_interface_language() {
        for (tag, language) in [
            ("zh-CN", Language::ZhCn),
            ("zh-SG", Language::ZhCn),
            ("zh-Hans-HK", Language::ZhCn),
            ("zh", Language::ZhCn),
            ("zh-TW", Language::ZhTw),
            ("zh-HK", Language::ZhTw),
            ("zh-MO", Language::ZhTw),
            ("zh-Hant", Language::ZhTw),
            ("zh_tw", Language::ZhTw),
            ("en-US", Language::En),
            ("en-GB", Language::En),
            ("ja-JP", Language::Ja),
            ("ko-KR", Language::Ko),
            ("fr-FR", Language::En),
            ("de", Language::En),
            ("", Language::En),
        ] {
            assert_eq!(from_tag(tag), language, "{tag}");
        }
    }

    #[test]
    fn following_windows_reads_its_language_and_falls_back_to_english() {
        assert_eq!(resolve(0, || Some("ja-JP".into())), Language::Ja);
        assert_eq!(resolve(0, || Some("pt-BR".into())), Language::En);
        assert_eq!(resolve(0, || None), Language::En);
        assert_eq!(
            resolve(2, || unreachable!("a fixed choice ignores Windows")),
            Language::ZhTw
        );
        assert_eq!(resolve(1, || Some("en-US".into())), Language::ZhCn);
    }

    #[test]
    fn the_choice_round_trips_and_anything_else_follows_windows() {
        let directory = tempfile::tempdir().unwrap();
        let control = directory.path().join("control");
        assert_eq!(load(&control), 0);
        for choice in [3, 5, 2, 1, 4, 0] {
            save(&control, choice).unwrap();
            assert_eq!(load(&control), choice);
        }
        fs::write(control.join(FILE), "fr").unwrap();
        assert_eq!(load(&control), 0);
    }

    #[test]
    fn arguments_fill_in_order_or_by_position() {
        assert_eq!(fill("{} / {}", &[&1, &"b"]), "1 / b");
        assert_eq!(fill("{1} before {0}", &[&"a", &"b"]), "b before a");
        assert_eq!(fill("{} {0} {}", &[&"x", &"y"]), "x x y");
        assert_eq!(fill("{missing} {}", &[&7]), "{missing} 7");
        assert_eq!(fill("no {", &[]), "no {");
    }

    #[test]
    fn lookups_pick_the_language_and_plural_form() {
        assert_eq!(lookup(MESSAGES, Language::ZhCn, "正在记录", 1), "正在记录");
        assert_eq!(lookup(MESSAGES, Language::En, "正在记录", 1), "Recording");
        assert_eq!(lookup(MESSAGES, Language::En, "{} 个应用", 1), "{} app");
        assert_eq!(lookup(MESSAGES, Language::En, "{} 个应用", 3), "{} apps");
        assert_eq!(
            lookup(MESSAGES, Language::Ja, "{} 个应用", 3),
            "{} 個のアプリ"
        );
        assert_eq!(lookup(MESSAGES, Language::En, "没有这句", 1), "没有这句");
    }

    /// The source texts a file passes to a macro or function, by its name and
    /// the opening parenthesis: the first string literal after each. A Slint
    /// context (`"context" => "text"`) is joined to its text as in a catalog.
    fn sources(code: &str, opening: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = code;
        while let Some(at) = rest.find(opening) {
            // The name inside a string, as in this test, is not a call.
            let quoted = rest[..at].ends_with('"');
            rest = &rest[at + opening.len()..];
            if quoted {
                continue;
            }
            let Some((mut text, after)) = literal(rest) else {
                break;
            };
            if let Some(source) = after.trim_start().strip_prefix("=>")
                && let Some((source, _)) = literal(source)
            {
                text = format!("{text}\u{4}{source}");
            }
            found.push(text);
        }
        found
    }

    /// The first string literal in `code`, and what follows it.
    fn literal(code: &str) -> Option<(String, &str)> {
        let quote = code.find('"')?;
        let mut text = String::new();
        let mut chars = code[quote + 1..].char_indices();
        while let Some((at, c)) = chars.next() {
            match c {
                '"' => return Some((text, &code[quote + 1 + at + 1..])),
                '\\' => match chars.next() {
                    Some((_, 'n')) => text.push('\n'),
                    Some((_, '\n')) => {
                        // A line continuation skips the next line's indent.
                        let tail = chars.as_str();
                        let skipped = tail.len() - tail.trim_start().len();
                        for _ in 0..tail[..skipped].chars().count() {
                            chars.next();
                        }
                    }
                    Some((_, other)) => text.push(other),
                    None => {}
                },
                c => text.push(c),
            }
        }
        None
    }

    /// The arguments a text uses, by position.
    fn placeholders(text: &str) -> std::collections::BTreeSet<String> {
        let mut used = std::collections::BTreeSet::new();
        let mut next = 0;
        for piece in text.split('{').skip(1) {
            let Some(name) = piece.split_once('}').map(|(name, _)| name) else {
                continue;
            };
            used.insert(if name.is_empty() {
                next += 1;
                (next - 1).to_string()
            } else {
                name.to_owned()
            });
        }
        used
    }

    fn files(directory: &str, extension: &str) -> Vec<(String, String)> {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join(directory);
        let mut files = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|found| found == extension))
            .map(|path| {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                (name, fs::read_to_string(&path).unwrap())
            })
            .collect::<Vec<_>>();
        files.sort();
        files
    }

    /// Every text that is shown, with the catalog that translates it.
    fn shown_texts() -> Vec<(String, &'static [Entry], String)> {
        let mut texts = Vec::new();
        for (file, code) in files("src", "rs") {
            for opening in ["tr!(", "trn!("] {
                for text in sources(&code, opening) {
                    texts.push((file.clone(), MESSAGES, text));
                }
            }
        }
        for detail in STORAGE_DETAILS {
            texts.push(("storage".to_owned(), MESSAGES, detail.to_owned()));
        }
        for (file, code) in files("ui", "slint") {
            for text in sources(&code, "@tr(") {
                texts.push((file.clone(), INTERFACE, text));
            }
        }
        texts
    }

    #[test]
    fn every_shown_text_is_translated_with_the_same_arguments() {
        let mut problems = Vec::new();
        for (file, catalog, text) in shown_texts() {
            let Ok(index) = catalog.binary_search_by(|(key, _)| (*key).cmp(text.as_str())) else {
                problems.push(format!("{file}: untranslated {text:?}"));
                continue;
            };
            for (column, forms) in catalog[index].1.iter().enumerate() {
                if forms.is_empty() {
                    problems.push(format!("{file}: no column {column} for {text:?}"));
                }
                for form in *forms {
                    // A plural form may leave the count to the number in its words.
                    let mut expected = placeholders(&text);
                    let found = placeholders(form);
                    if found.contains("n") || expected.contains("n") {
                        expected.insert("n".to_owned());
                    }
                    if found.iter().any(|name| !expected.contains(name))
                        || expected
                            .iter()
                            .any(|name| name != "n" && !found.contains(name))
                    {
                        problems.push(format!("{file}: {text:?} became {form:?}"));
                    }
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn failures_and_work_in_progress_read_alike_in_every_language() {
        let mut problems = Vec::new();
        // Toasts show what Rust writes; a storage detail is always part of a
        // status that already says it failed.
        for (file, catalog, text) in shown_texts() {
            if file == "storage" || file.ends_with(".slint") {
                continue;
            }
            let Ok(index) = catalog.binary_search_by(|(key, _)| (*key).cmp(text.as_str())) else {
                continue;
            };
            for form in catalog[index].1.iter().flat_map(|forms| forms.iter()) {
                if reads_as_failure(&text) != reads_as_failure(form)
                    || text.ends_with('…') != form.ends_with('…')
                {
                    problems.push(format!("{text:?} became {form:?}"));
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    #[test]
    fn storage_details_read_in_the_current_language() {
        // Only the source language is safe to set while other tests run.
        assert_eq!(
            localize("database integrity check failed: 备份路径无效"),
            "database integrity check failed: 备份路径无效"
        );
        assert_eq!(
            lookup(MESSAGES, Language::En, "备份路径无效", 1),
            "The backup path is invalid"
        );
    }
}
