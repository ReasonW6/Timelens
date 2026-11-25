//! Application categories: their names and colors, and the settings page that
//! edits them and their folder rules.
use crate::i18n::{tr, trn};
use crate::{AppWindow, CategoryItem, CategoryState};
use anyhow::{Result, anyhow};
use slint::{Color, ComponentHandle, ModelRc, VecModel};
use std::{
    rc::Rc,
    sync::{Arc, Mutex},
};
use timelens_storage::{CATEGORY_NAME_LIMIT, Category, Storage, normalize_folder};

/// The colors categories wear, by color index; each reads on light and dark.
const COLORS: [(u8, u8, u8); 10] = [
    (0x4a, 0x7f, 0xd6), // blue
    (0x3f, 0x9c, 0x58), // green
    (0xe0, 0x72, 0x3c), // orange
    (0xd2, 0x52, 0x78), // rose
    (0x16, 0x9a, 0x94), // teal
    (0x8a, 0x6c, 0xd8), // violet
    (0xd0, 0x8f, 0x28), // amber
    (0x2f, 0x96, 0xc0), // cyan
    (0xb5, 0x5a, 0xc8), // orchid
    (0x7c, 0x9a, 0x2f), // olive
];

pub fn color(category: &Category) -> Color {
    let (r, g, b) = COLORS[category.color as usize % COLORS.len()];
    Color::from_rgb_u8(r, g, b)
}

/// The color of applications in no category.
pub fn uncategorized_color() -> Color {
    Color::from_rgb_u8(0x9a, 0xa0, 0xac)
}

/// A category's name: the user's, or a preset's own in the interface language.
pub fn name(category: &Category) -> String {
    if let Some(name) = &category.name {
        return name.clone();
    }
    match category.preset.as_deref() {
        Some("work") => tr!("工作"),
        Some("study") => tr!("学习"),
        Some("entertainment") => tr!("娱乐"),
        Some("social") => tr!("社交"),
        Some("tools") => tr!("工具"),
        _ => tr!("未命名"),
    }
    .to_owned()
}

fn lock(storage: &Arc<Mutex<Storage>>) -> Result<std::sync::MutexGuard<'_, Storage>> {
    storage.lock().map_err(|_| anyhow!(tr!("存储锁不可用")))
}

/// A new name checked against the limit and the other categories' names.
fn checked_name(text: &str, categories: &[Category], renamed: Option<i64>) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(anyhow!(tr!("请输入分类名称")));
    }
    if text.chars().count() > CATEGORY_NAME_LIMIT {
        return Err(anyhow!(tr!("分类名称最多 {} 个字", CATEGORY_NAME_LIMIT)));
    }
    if categories
        .iter()
        .any(|c| Some(c.id) != renamed && name(c).to_lowercase() == text.to_lowercase())
    {
        return Err(anyhow!(tr!("已有名为「{}」的分类", text)));
    }
    Ok(text.to_owned())
}

fn show(window: &AppWindow, storage: &Storage) -> Result<()> {
    let items = storage
        .categories()?
        .iter()
        .map(|category| CategoryItem {
            id: category.id as i32,
            name: name(category).into(),
            tint: color(category),
            folders: Rc::new(VecModel::from(
                category
                    .folders
                    .iter()
                    .map(|folder| folder.into())
                    .collect::<Vec<slint::SharedString>>(),
            ))
            .into(),
            summary: if category.folders.is_empty() {
                tr!("没有目录规则").into()
            } else {
                trn!(
                    category.folders.len(),
                    "{} 个目录规则",
                    category.folders.len()
                )
                .into()
            },
        })
        .collect::<Vec<_>>();
    window
        .global::<CategoryState>()
        .set_items(ModelRc::new(VecModel::from(items)));
    Ok(())
}

/// Apply a change, then show the categories again and say how it went.
fn apply(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    change: impl FnOnce(&Storage) -> Result<String>,
) {
    let state = window.global::<CategoryState>();
    let result = lock(storage).and_then(|storage| {
        let message = change(&storage)?;
        show(window, &storage)?;
        Ok(message)
    });
    match result {
        Ok(message) => {
            state.set_confirm_delete(-1);
            crate::trend_ui::invalidate();
            state.set_status(message.into());
        }
        Err(error) => state.set_status(tr!("保存失败：{}", error).into()),
    }
}

pub fn install(window: &AppWindow, storage: Arc<Mutex<Storage>>) {
    let state = window.global::<CategoryState>();
    {
        let weak = window.as_weak();
        let storage = storage.clone();
        state.on_opened(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let state = window.global::<CategoryState>();
            state.set_confirm_delete(-1);
            if let Err(error) = lock(&storage).and_then(|storage| show(&window, &storage)) {
                state.set_status(tr!("读取分类失败：{}", error).into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = storage.clone();
        state.on_add(move |text| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            apply(&window, &storage, |storage| {
                let text = checked_name(&text, &storage.categories()?, None)?;
                storage.add_category(&text)?;
                window.global::<CategoryState>().set_new_name("".into());
                Ok(tr!("已新建分类「{}」", text))
            });
        });
    }
    {
        let weak = window.as_weak();
        let storage = storage.clone();
        state.on_rename(move |id, text| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            apply(&window, &storage, |storage| {
                let categories = storage.categories()?;
                let category = categories
                    .iter()
                    .find(|c| c.id == i64::from(id))
                    .ok_or_else(|| anyhow!(tr!("这个分类已不存在")))?;
                // A preset given no name takes its own back.
                if text.trim().is_empty() && category.preset.is_some() {
                    storage.rename_category(category.id, "")?;
                    let mut own = category.clone();
                    own.name = None;
                    return Ok(tr!("已恢复默认名称「{}」", name(&own)));
                }
                let text = checked_name(&text, &categories, Some(category.id))?;
                storage.rename_category(category.id, &text)?;
                Ok(tr!("已改名为「{}」", text))
            });
        });
    }
    {
        let weak = window.as_weak();
        let storage = storage.clone();
        state.on_remove(move |id| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            apply(&window, &storage, |storage| {
                let removed = storage
                    .categories()?
                    .iter()
                    .find(|c| c.id == i64::from(id))
                    .map(name)
                    .ok_or_else(|| anyhow!(tr!("这个分类已不存在")))?;
                storage.delete_category(i64::from(id))?;
                Ok(tr!("已删除分类「{}」", removed))
            });
        });
    }
    {
        let weak = window.as_weak();
        let storage = storage.clone();
        state.on_remove_folder(move |folder| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            apply(&window, &storage, |storage| {
                storage.remove_category_folder(&folder)?;
                Ok(tr!("已移除目录 {}", folder))
            });
        });
    }
    let weak = window.as_weak();
    state.on_add_folder(move |id| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let state = window.global::<CategoryState>();
        if state.get_busy() {
            return;
        }
        state.set_busy(true);
        let weak = window.as_weak();
        let storage = storage.clone();
        // The picker waits for the user, so it runs off the UI thread.
        std::thread::spawn(move || {
            let chosen = crate::data_ui::choose_folder(tr!("选择要归入分类的目录"));
            let _ = weak.upgrade_in_event_loop(move |window| {
                window.global::<CategoryState>().set_busy(false);
                let folder = match chosen {
                    Ok(Some(folder)) => folder,
                    Ok(None) => return,
                    Err(error) => {
                        window
                            .global::<CategoryState>()
                            .set_status(tr!("保存失败：{}", error).into());
                        return;
                    }
                };
                apply(&window, &storage, |storage| {
                    let folder = normalize_folder(&folder.to_string_lossy())
                        .ok_or_else(|| anyhow!(tr!("请选择一个目录")))?;
                    let categories = storage.categories()?;
                    if let Some(owner) = categories.iter().find(|c| c.folders.contains(&folder)) {
                        return Err(if owner.id == i64::from(id) {
                            anyhow!(tr!("这个目录已经在「{}」里", name(owner)))
                        } else {
                            anyhow!(tr!("这个目录已归入「{}」，请先从那里移除", name(owner)))
                        });
                    }
                    let target = categories
                        .iter()
                        .find(|c| c.id == i64::from(id))
                        .ok_or_else(|| anyhow!(tr!("这个分类已不存在")))?;
                    storage.add_category_folder(target.id, &folder)?;
                    Ok(tr!(
                        "已添加目录 {}，其中的应用会自动归入「{}」",
                        folder,
                        name(target)
                    ))
                });
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::{checked_name, name};
    use timelens_storage::Category;

    fn category(id: i64, preset: Option<&str>, own: Option<&str>) -> Category {
        Category {
            id,
            preset: preset.map(Into::into),
            name: own.map(Into::into),
            color: 0,
            folders: Vec::new(),
        }
    }

    #[test]
    fn presets_are_named_until_renamed_and_names_stay_distinct() {
        let categories = [
            category(1, Some("work"), None),
            category(2, Some("study"), Some("Reading")),
        ];
        assert_eq!(name(&categories[0]), "工作");
        assert_eq!(name(&categories[1]), "Reading");
        assert_eq!(checked_name("  游戏 ", &categories, None).unwrap(), "游戏");
        assert!(checked_name("工作", &categories, None).is_err());
        assert!(checked_name("reading", &categories, None).is_err());
        // A category keeps its own name.
        assert!(checked_name("Reading", &categories, Some(2)).is_ok());
        assert!(checked_name(" ", &categories, None).is_err());
        assert!(checked_name(&"长".repeat(41), &categories, None).is_err());
    }
}
