use crate::{AppWindow, ButtonHeat, CollectionState, KeyCell, SystemRow, UiState};
use anyhow::{Context, Result, anyhow};
use chrono::TimeZone;
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};
use timelens_storage::Storage;
#[derive(Default)]
struct Data {
    apps: Vec<(String, String)>,
    links: Vec<(String, String)>,
}
fn lock(s: &Arc<Mutex<Storage>>) -> Result<std::sync::MutexGuard<'_, Storage>> {
    s.lock().map_err(|_| anyhow!("存储锁不可用"))
}
fn strings(values: impl IntoIterator<Item = String>) -> ModelRc<slint::SharedString> {
    Rc::new(VecModel::from(
        values.into_iter().map(Into::into).collect::<Vec<_>>(),
    ))
    .into()
}
/// Where an application lives, to tell apart two with the same name.
fn origin(identity: &str) -> String {
    let Some(path) = identity.strip_prefix("path:") else {
        return identity
            .split_once(':')
            .map_or(identity, |(_, rest)| rest)
            .to_owned();
    };
    std::path::Path::new(path)
        .parent()
        .and_then(|folder| folder.file_name())
        .map_or_else(
            || path.to_owned(),
            |folder| folder.to_string_lossy().into_owned(),
        )
}
fn refresh(w: &AppWindow, d: &mut Data, s: &Storage) -> Result<()> {
    let g = w.global::<CollectionState>();
    let apps = s
        .known_applications()?
        .into_iter()
        .map(|(identity, name)| {
            let name = crate::app_icon::display_name(&identity, &name);
            (identity, name)
        })
        .collect::<Vec<_>>();
    // Listed by name; a name shared by several gets their folders added.
    d.apps = apps
        .iter()
        .map(|(identity, name)| {
            let shared = apps.iter().filter(|(_, other)| other == name).count() > 1;
            let shown = if shared {
                format!("{name}（{}）", origin(identity))
            } else {
                name.clone()
            };
            (identity.clone(), shown)
        })
        .collect();
    d.apps.sort_by(|a, b| {
        a.1.to_lowercase()
            .cmp(&b.1.to_lowercase())
            .then_with(|| a.0.cmp(&b.0))
    });
    d.links = s.merge_links()?;
    g.set_apps(strings(d.apps.iter().map(|(_, name)| name.clone())));
    g.set_links(strings(d.links.iter().map(|(a, b)| {
        let name = |id: &str| {
            d.apps
                .iter()
                .find(|(i, _)| i == id)
                .map_or(id.to_owned(), |(_, n)| n.clone())
        };
        format!("{} ↔ {}", name(a), name(b))
    })));
    let p = s.collection_policy()?;
    g.set_paused(p.paused);
    let names = |ids: &std::collections::BTreeSet<String>| {
        d.apps
            .iter()
            .filter(|(id, _)| ids.contains(id))
            .map(|(_, name)| name.clone())
            .collect::<Vec<_>>()
            .join("、")
    };
    g.set_rules(
        format!(
            "活动名单：{}\n输入名单：{}\n快照名单：{}",
            names(&p.activity),
            names(&p.input),
            names(&s.snapshot_exclusions()?.into_iter().collect())
        )
        .into(),
    );
    Ok(())
}
fn select(w: &AppWindow, d: &Data, s: &Storage, index: i32) -> Result<()> {
    let (id, name) = d.apps.get(index as usize).context("请选择应用")?;
    let p = s.collection_policy()?;
    let g = w.global::<CollectionState>();
    g.set_app_index(index);
    g.set_app_name(name.into());
    g.set_activity(p.activity.contains(id));
    g.set_input(p.input.contains(id));
    g.set_snapshots(s.snapshot_exclusions()?.contains(id));
    let (windows, reports) = s.application_deletion_impact(id)?;
    g.set_impact(
        format!("将删除「{name}」的 {windows} 个窗口实例和 {reports} 份本地报告。").into(),
    );
    g.set_delete_confirmed(false);
    g.set_merge_confirmed(false);
    g.set_merge_index(-1);
    g.set_merge_preview("".into());
    Ok(())
}
/// The rules two applications share once merged: each list takes both.
fn merge_preview(d: &Data, s: &Storage, first: i32, second: i32) -> Result<String> {
    let (a, a_name) = d.apps.get(first as usize).context("请选择应用")?;
    let (b, b_name) = d.apps.get(second as usize).context("请选择另一个应用")?;
    let p = s.collection_policy()?;
    let shots = s.snapshot_exclusions()?;
    let rule = |label: &str, has: &dyn Fn(&str) -> bool| match (has(a), has(b)) {
        (false, false) => format!("{label} ✗"),
        (true, true) => format!("{label} ✓"),
        (true, false) => format!("{label} ✓（来自「{a_name}」）"),
        (false, true) => format!("{label} ✓（来自「{b_name}」）"),
    };
    Ok(format!(
        "合并后共用规则：{} · {} · {}。",
        rule("不记录活动", &|id| p.activity.contains(id)),
        rule("不记录输入", &|id| p.input.contains(id)),
        rule("不拍摄快照", &|id| shots.iter().any(|shot| shot == id)),
    ))
}
pub fn install(
    w: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    timeline: Rc<RefCell<UiState>>,
) -> Result<Timer> {
    let data = Rc::new(RefCell::new(Data::default()));
    let (sender, receiver) = mpsc::channel::<(i32, String, Result<Option<String>, String>)>();
    refresh(w, &mut data.borrow_mut(), &*lock(&storage)?)?;
    {
        let weak = w.as_weak();
        let data = data.clone();
        let storage = storage.clone();
        w.global::<CollectionState>().on_merge_chosen(move |other| {
            let Some(w) = weak.upgrade() else {
                return;
            };
            let g = w.global::<CollectionState>();
            let first = g.get_app_index();
            let preview = if other == first {
                "不能和自己合并，请选择另一个应用。".to_owned()
            } else {
                lock(&storage)
                    .and_then(|s| merge_preview(&data.borrow(), &s, first, other))
                    .unwrap_or_else(|e| format!("读取规则失败：{e}"))
            };
            g.set_merge_preview(preview.into());
        });
    }
    {
        let weak = w.as_weak();
        let data = data.clone();
        let storage = storage.clone();
        let timeline = timeline.clone();
        w.global::<CollectionState>().on_opened(move || {
            if let Some(w) = weak.upgrade() {
                let result = (|| {
                    let s = lock(&storage)?;
                    let mut d = data.borrow_mut();
                    refresh(&w, &mut d, &s)?;
                    let index = timeline
                        .borrow()
                        .selected_identity
                        .as_ref()
                        .and_then(|id| d.apps.iter().position(|(i, _)| i == id))
                        .unwrap_or(0);
                    if !d.apps.is_empty() {
                        select(&w, &d, &s, index as i32)?;
                    }
                    Ok::<_, anyhow::Error>(())
                })();
                if let Err(e) = result {
                    w.global::<CollectionState>()
                        .set_status(format!("读取统计失败：{e}").into());
                }
            }
        });
    }
    {
        let weak = w.as_weak();
        let data = data.clone();
        let storage = storage.clone();
        w.global::<CollectionState>().on_choose(move |index| {
            if let Some(w) = weak.upgrade()
                && let Err(e) = lock(&storage).and_then(|s| select(&w, &data.borrow(), &s, index))
            {
                w.global::<CollectionState>()
                    .set_status(format!("读取统计失败：{e}").into());
            }
        });
    }
    let weak = w.as_weak();
    let state_data = data.clone();
    let worker_storage = storage.clone();
    w.global::<CollectionState>().on_action(move |action| {
        let Some(w) = weak.upgrade() else {
            return;
        };
        let g = w.global::<CollectionState>();
        if g.get_busy() {
            return;
        }
        if action == 7 {
            let result = lock(&worker_storage).and_then(|s| statistics(&w, &s, &timeline.borrow()));
            if let Err(e) = result {
                g.set_status(format!("读取统计失败：{e}").into());
            }
            return;
        }
        let d = state_data.borrow();
        let id = d
            .apps
            .get(g.get_app_index() as usize)
            .map(|(id, _)| id.clone());
        let other = d
            .apps
            .get(g.get_merge_index() as usize)
            .map(|(id, _)| id.clone());
        let link = d.links.get(g.get_link_index() as usize).cloned();
        let name = g.get_app_name().to_string();
        drop(d);
        let flags = (
            g.get_paused(),
            g.get_activity(),
            g.get_input(),
            g.get_snapshots(),
        );
        let confirmed = g.get_delete_confirmed();
        let merge_confirmed = g.get_merge_confirmed();
        let storage = worker_storage.clone();
        let sender = sender.clone();
        g.set_busy(true);
        std::thread::spawn(move || {
            let result: Result<Option<String>> = (|| {
                if action == 1 {
                    let Some(path) = crate::data_ui::choose_file(false, "exe")? else {
                        return Ok(None);
                    };
                    return Ok(Some(format!(
                        "path:{}",
                        path.to_string_lossy().replace('/', "\\").to_lowercase()
                    )));
                }
                crate::snapshot::run_heavy_task(|| {
                    let _collector = if action == 5 {
                        let control = lock(&storage)?.control_directory().to_path_buf();
                        Some(crate::data_ui::pause_collector(&control)?)
                    } else {
                        None
                    };
                    let s = lock(&storage)?;
                    match action {
                        0 => {
                            let mut p = s.collection_policy()?;
                            p.paused = flags.0;
                            s.set_collection_policy(p)?;
                        }
                        2 => {
                            let id = id.context("请选择应用")?;
                            let mut p = s.collection_policy()?;
                            for member in s.merged_members(&id)? {
                                for (list, enabled) in
                                    [(&mut p.activity, flags.1), (&mut p.input, flags.2)]
                                {
                                    if enabled {
                                        list.insert(member.clone());
                                    } else {
                                        list.remove(&member);
                                    }
                                }
                                s.set_snapshot_excluded(&member, flags.3, crate::unix_time_ms())?;
                            }
                            s.set_collection_policy(p)?;
                        }
                        3 => {
                            if !merge_confirmed {
                                return Err(anyhow!("请确认三份规则各自取并集"));
                            }
                            s.set_application_merge(
                                &id.context("请选择应用")?,
                                &other.context("请选择另一个应用")?,
                                true,
                            )?;
                            crate::history::invalidate();
                        }
                        4 => {
                            let (a, b) = link.context("请选择合并关联")?;
                            s.set_application_merge(&a, &b, false)?;
                            crate::history::invalidate();
                        }
                        5 => {
                            if !confirmed {
                                return Err(anyhow!("请确认删除影响"));
                            }
                            s.delete_application_history(&id.context("请选择应用")?)?;
                            crate::data_ui::dataset_changed();
                        }
                        _ => return Err(anyhow!("未知操作")),
                    }
                    Ok(None)
                })
            })();
            let _ = sender.send((action, name, result.map_err(|e| e.to_string())));
        });
    });
    let weak = w.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(w) = weak.upgrade() else {
            return;
        };
        let g = w.global::<CollectionState>();
        while let Ok((action, name, result)) = receiver.try_recv() {
            g.set_busy(false);
            if action != 1
                && let Ok(s) = storage.try_lock()
            {
                // Switches show what is saved, not what was asked for, and a
                // deleted application gives its place to the next one.
                let mut d = data.borrow_mut();
                if refresh(&w, &mut d, &s).is_ok() && !d.apps.is_empty() {
                    let index = g.get_app_index().clamp(0, d.apps.len() as i32 - 1);
                    if g.get_app_index() >= 0 || action == 5 {
                        let _ = select(&w, &d, &s, index);
                    }
                }
            }
            match result {
                Err(e) => {
                    let what = if action == 0 {
                        "切换记录状态失败"
                    } else {
                        "保存失败"
                    };
                    g.set_status(format!("{what}：{e}").into());
                }
                Ok(None) if action == 2 => {
                    g.set_status(
                        format!("已更新「{name}」的规则，立即生效，不影响已有记录").into(),
                    );
                }
                Ok(None) if action == 3 => {
                    g.set_status(format!("已合并「{name}」，原始记录不改写").into())
                }
                Ok(None) if action == 4 => g.set_status("已解除合并，恢复合并前的规则".into()),
                Ok(None) if action == 5 => g.set_status(format!("已删除「{name}」的历史").into()),
                Ok(None) if action == 0 => g.set_status(
                    if g.get_paused() {
                        "已暂停记录"
                    } else {
                        "已继续记录，从现在开始记录"
                    }
                    .into(),
                ),
                Ok(added) => {
                    if let Ok(s) = storage.try_lock() {
                        let mut d = data.borrow_mut();
                        let _ = refresh(&w, &mut d, &s);
                        if let Some(id) = added {
                            if !d.apps.iter().any(|(i, _)| i == &id) {
                                let name = std::path::Path::new(id.trim_start_matches("path:"))
                                    .file_name()
                                    .map_or(id.clone(), |s| s.to_string_lossy().into_owned());
                                let name = crate::app_icon::display_name(&id, &name);
                                d.apps.push((id.clone(), name));
                                g.set_apps(strings(d.apps.iter().map(|(_, n)| n.clone())));
                            }
                            let index = d.apps.iter().position(|(i, _)| i == &id).unwrap_or(0);
                            let _ = select(&w, &d, &s, index as i32);
                        }
                    }
                }
            }
        }
    });
    Ok(timer)
}
/// The local dates a range touches, both included, and whether it covers them
/// whole; `None` stands for all records.
fn range_dates(range: Option<&UiState>) -> (String, String, bool) {
    let Some(range) = range else {
        return ("0000-01-01".into(), "9999-12-31".into(), true);
    };
    let date = |timestamp: i64| {
        chrono::Local
            .timestamp_millis_opt(timestamp)
            .single()
            .map(|t| t.date_naive())
    };
    let start = range.range_started_utc_ms;
    let end = range.range_ended_utc_ms.max(start + 1);
    let (Some(first), Some(last)) = (date(start), date(end - 1)) else {
        return (String::new(), String::new(), false);
    };
    let midnight = |day: chrono::NaiveDate| {
        day.and_hms_opt(0, 0, 0)
            .and_then(|t| chrono::Local.from_local_datetime(&t).earliest())
            .map(|t| t.timestamp_millis())
    };
    // Whole days run from a midnight to a midnight; today counts while it runs.
    let whole = midnight(first) == Some(start)
        && last.succ_opt().and_then(midnight) == Some(crate::timeline_ui::chosen_end(range));
    (
        first.format("%Y-%m-%d").to_string(),
        last.format("%Y-%m-%d").to_string(),
        whole,
    )
}

fn date_label(date: &str) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| d.format("%-m月%-d日").to_string())
        .unwrap_or_else(|_| date.to_owned())
}

fn statistics(w: &AppWindow, s: &Storage, range: &UiState) -> Result<()> {
    let g = w.global::<CollectionState>();
    let all = w.get_stats_all();
    let (first, last, whole) = range_dates((!all).then_some(range));
    if first.is_empty() {
        anyhow::bail!("所选时段不可用");
    }
    g.set_scope(
        if all {
            "全部记录".to_owned()
        } else if first == last {
            date_label(&first)
        } else {
            format!("{}–{}", date_label(&first), date_label(&last))
        }
        .into(),
    );
    g.set_partial_day(!whole);
    let frequencies: std::collections::BTreeMap<_, _> = s
        .physical_key_frequencies_between(&first, &last)?
        .into_iter()
        .collect();
    let max = KEYBOARD
        .iter()
        .filter_map(|(code, ..)| frequencies.get(code).copied())
        .max()
        .unwrap_or(1)
        .max(1) as f32;
    let keys = KEYBOARD
        .iter()
        .map(|&(code, label, x, y, w, h)| {
            let count = frequencies.get(&code).copied().unwrap_or(0);
            KeyCell {
                label: label.into(),
                count: crate::grouped_count(count).into(),
                strength: (count as f32 / max).sqrt(),
                x,
                y,
                w,
                h,
            }
        })
        .collect::<Vec<_>>();
    let unplaced = frequencies
        .iter()
        .filter(|(code, _)| !KEYBOARD.iter().any(|(placed, ..)| placed == *code))
        .map(|(_, count)| count)
        .sum::<u64>();
    g.set_unplaced_keys(if unplaced == 0 {
        "".into()
    } else {
        format!(
            "另有 {} 次按键不在这张键盘图上，例如 Home、Delete 等编辑键或输入法切换键。",
            crate::grouped_count(unplaced)
        )
        .into()
    });
    let [_, left, middle, right] = s.input_totals_between(&first, &last)?;
    let most = left.max(middle).max(right).max(1) as f32;
    let heat = |count: u64| ButtonHeat {
        count: crate::grouped_count(count).into(),
        strength: (count as f32 / most).sqrt(),
    };
    g.set_mouse_left(heat(left));
    g.set_mouse_middle(heat(middle));
    g.set_mouse_right(heat(right));
    g.set_keys(Rc::new(VecModel::from(keys)).into());
    let totals = s.permanent_input_totals()?;
    let [keyboard, left, middle, right] = totals.map(crate::grouped_count);
    g.set_totals(format!("键盘 {keyboard} · 左键 {left} · 中键 {middle} · 右键 {right}").into());
    // All records start with the first one, not at the query's lower bound.
    let now = crate::unix_time_ms();
    let (start, end) = if all {
        // System states may start before the first application window.
        let first = [s.earliest_activity_utc_ms()?, s.earliest_system_utc_ms()?]
            .into_iter()
            .flatten()
            .min();
        (first.unwrap_or(now), now)
    } else {
        (range.range_started_utc_ms, range.range_ended_utc_ms)
    };
    let length = end.saturating_sub(start).max(1) as f64;
    let mut totals = s
        .system_totals(start, end)?
        .into_iter()
        .filter(|(_, ms)| *ms > 0)
        .collect::<Vec<_>>();
    totals.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let rows = totals
        .into_iter()
        .map(|(kind, ms)| {
            let ratio = (ms as f64 / length).clamp(0.0, 1.0);
            SystemRow {
                label: system_label(&kind).into(),
                duration: crate::format_duration(ms).into(),
                share: if ratio > 0.0 && ratio < 0.001 {
                    "<0.1%".into()
                } else {
                    format!("{:.1}%", ratio * 100.0).into()
                },
                ratio: ratio as f32,
            }
        })
        .collect::<Vec<_>>();
    g.set_system(Rc::new(VecModel::from(rows)).into());
    // Instants take no time; they are counted instead.
    g.set_system_marks(
        s.system_marks(start, end)?
            .into_iter()
            .map(|(kind, count)| {
                format!("{} {} 次", system_label(&kind), crate::grouped_count(count))
            })
            .collect::<Vec<_>>()
            .join(" · ")
            .into(),
    );
    Ok(())
}
fn system_label(kind: &str) -> &str {
    match kind {
        "desktop_idle" => "桌面空闲",
        "locked" => "锁屏",
        "sleep" => "睡眠",
        "global_pause" => "暂停",
        "privacy_exclusion" => "活动排除",
        "session_disconnected" => "会话断开",
        "secure_desktop" => "安全桌面",
        "clock_discontinuity" => "系统时间跳变",
        "system_end" => "系统正常结束",
        _ => kind,
    }
}

/// A compact 93-key board: the main block with the arrows tucked into its
/// lower right corner, then the numeric keypad. Each key is its scan code
/// (0x100 marks an extended key), label, and position and size in key units.
const KEYBOARD: &[(u32, &str, f32, f32, f32, f32)] = &[
    (0x01, "Esc", 0.0, 0.0, 1.0, 1.0),
    (0x3b, "F1", 2.0, 0.0, 1.0, 1.0),
    (0x3c, "F2", 3.0, 0.0, 1.0, 1.0),
    (0x3d, "F3", 4.0, 0.0, 1.0, 1.0),
    (0x3e, "F4", 5.0, 0.0, 1.0, 1.0),
    (0x3f, "F5", 6.5, 0.0, 1.0, 1.0),
    (0x40, "F6", 7.5, 0.0, 1.0, 1.0),
    (0x41, "F7", 8.5, 0.0, 1.0, 1.0),
    (0x42, "F8", 9.5, 0.0, 1.0, 1.0),
    (0x43, "F9", 11.0, 0.0, 1.0, 1.0),
    (0x44, "F10", 12.0, 0.0, 1.0, 1.0),
    (0x57, "F11", 13.0, 0.0, 1.0, 1.0),
    (0x58, "F12", 14.0, 0.0, 1.0, 1.0),
    (0x29, "`", 0.0, 1.5, 1.0, 1.0),
    (0x02, "1", 1.0, 1.5, 1.0, 1.0),
    (0x03, "2", 2.0, 1.5, 1.0, 1.0),
    (0x04, "3", 3.0, 1.5, 1.0, 1.0),
    (0x05, "4", 4.0, 1.5, 1.0, 1.0),
    (0x06, "5", 5.0, 1.5, 1.0, 1.0),
    (0x07, "6", 6.0, 1.5, 1.0, 1.0),
    (0x08, "7", 7.0, 1.5, 1.0, 1.0),
    (0x09, "8", 8.0, 1.5, 1.0, 1.0),
    (0x0a, "9", 9.0, 1.5, 1.0, 1.0),
    (0x0b, "0", 10.0, 1.5, 1.0, 1.0),
    (0x0c, "−", 11.0, 1.5, 1.0, 1.0),
    (0x0d, "=", 12.0, 1.5, 1.0, 1.0),
    (0x0e, "Backspace", 13.0, 1.5, 2.0, 1.0),
    (0x145, "Num", 15.75, 1.5, 1.0, 1.0),
    (0x135, "/", 16.75, 1.5, 1.0, 1.0),
    (0x37, "*", 17.75, 1.5, 1.0, 1.0),
    (0x4a, "−", 18.75, 1.5, 1.0, 1.0),
    (0x0f, "Tab", 0.0, 2.5, 1.5, 1.0),
    (0x10, "Q", 1.5, 2.5, 1.0, 1.0),
    (0x11, "W", 2.5, 2.5, 1.0, 1.0),
    (0x12, "E", 3.5, 2.5, 1.0, 1.0),
    (0x13, "R", 4.5, 2.5, 1.0, 1.0),
    (0x14, "T", 5.5, 2.5, 1.0, 1.0),
    (0x15, "Y", 6.5, 2.5, 1.0, 1.0),
    (0x16, "U", 7.5, 2.5, 1.0, 1.0),
    (0x17, "I", 8.5, 2.5, 1.0, 1.0),
    (0x18, "O", 9.5, 2.5, 1.0, 1.0),
    (0x19, "P", 10.5, 2.5, 1.0, 1.0),
    (0x1a, "[", 11.5, 2.5, 1.0, 1.0),
    (0x1b, "]", 12.5, 2.5, 1.0, 1.0),
    (0x2b, "\\", 13.5, 2.5, 1.5, 1.0),
    (0x47, "7", 15.75, 2.5, 1.0, 1.0),
    (0x48, "8", 16.75, 2.5, 1.0, 1.0),
    (0x49, "9", 17.75, 2.5, 1.0, 1.0),
    (0x4e, "+", 18.75, 2.5, 1.0, 2.0),
    (0x3a, "Caps", 0.0, 3.5, 1.75, 1.0),
    (0x1e, "A", 1.75, 3.5, 1.0, 1.0),
    (0x1f, "S", 2.75, 3.5, 1.0, 1.0),
    (0x20, "D", 3.75, 3.5, 1.0, 1.0),
    (0x21, "F", 4.75, 3.5, 1.0, 1.0),
    (0x22, "G", 5.75, 3.5, 1.0, 1.0),
    (0x23, "H", 6.75, 3.5, 1.0, 1.0),
    (0x24, "J", 7.75, 3.5, 1.0, 1.0),
    (0x25, "K", 8.75, 3.5, 1.0, 1.0),
    (0x26, "L", 9.75, 3.5, 1.0, 1.0),
    (0x27, ";", 10.75, 3.5, 1.0, 1.0),
    (0x28, "'", 11.75, 3.5, 1.0, 1.0),
    (0x1c, "Enter", 12.75, 3.5, 2.25, 1.0),
    (0x4b, "4", 15.75, 3.5, 1.0, 1.0),
    (0x4c, "5", 16.75, 3.5, 1.0, 1.0),
    (0x4d, "6", 17.75, 3.5, 1.0, 1.0),
    (0x2a, "Shift", 0.0, 4.5, 2.25, 1.0),
    (0x2c, "Z", 2.25, 4.5, 1.0, 1.0),
    (0x2d, "X", 3.25, 4.5, 1.0, 1.0),
    (0x2e, "C", 4.25, 4.5, 1.0, 1.0),
    (0x2f, "V", 5.25, 4.5, 1.0, 1.0),
    (0x30, "B", 6.25, 4.5, 1.0, 1.0),
    (0x31, "N", 7.25, 4.5, 1.0, 1.0),
    (0x32, "M", 8.25, 4.5, 1.0, 1.0),
    (0x33, ",", 9.25, 4.5, 1.0, 1.0),
    (0x34, ".", 10.25, 4.5, 1.0, 1.0),
    (0x35, "/", 11.25, 4.5, 1.0, 1.0),
    (0x36, "Shift", 12.25, 4.5, 1.75, 1.0),
    (0x148, "↑", 14.5, 4.5, 1.0, 1.0),
    (0x4f, "1", 15.75, 4.5, 1.0, 1.0),
    (0x50, "2", 16.75, 4.5, 1.0, 1.0),
    (0x51, "3", 17.75, 4.5, 1.0, 1.0),
    (0x11c, "Enter", 18.75, 4.5, 1.0, 2.0),
    (0x1d, "Ctrl", 0.0, 5.5, 1.25, 1.0),
    (0x15b, "Win", 1.25, 5.5, 1.25, 1.0),
    (0x38, "Alt", 2.5, 5.5, 1.25, 1.0),
    (0x39, "Space", 3.75, 5.5, 6.25, 1.0),
    (0x138, "Alt", 10.0, 5.5, 1.5, 1.0),
    (0x11d, "Ctrl", 11.5, 5.5, 1.5, 1.0),
    (0x14b, "←", 13.5, 5.5, 1.0, 1.0),
    (0x150, "↓", 14.5, 5.5, 1.0, 1.0),
    (0x14d, "→", 15.5, 5.5, 1.0, 1.0),
    (0x52, "0", 16.75, 5.5, 1.0, 1.0),
    (0x53, ".", 17.75, 5.5, 1.0, 1.0),
];

#[cfg(test)]
mod tests {
    use super::{KEYBOARD, range_dates};
    use crate::UiState;
    use chrono::TimeZone;

    #[test]
    fn range_dates_cover_whole_days_and_flag_parts_of_days() {
        let at = |day: u32, hour: u32| {
            chrono::Local
                .with_ymd_and_hms(2026, 9, day, hour, 0, 0)
                .earliest()
                .unwrap()
                .timestamp_millis()
        };
        let mut range = UiState::recent_hours(24);
        range.horizon_started_utc_ms = at(5, 0);
        range.horizon_ended_utc_ms = at(6, 0);
        range.range_started_utc_ms = at(5, 0);
        // A day still running ends at the clock, yet counts whole.
        range.range_ended_utc_ms = at(5, 10);
        assert_eq!(
            range_dates(Some(&range)),
            ("2026-09-05".into(), "2026-09-05".into(), true)
        );
        // Part of a past day picked from the timeline or a summary.
        range.follow_now = false;
        assert!(!range_dates(Some(&range)).2);
        range.range_started_utc_ms = at(5, 9);
        range.horizon_ended_utc_ms = at(5, 18);
        range.range_ended_utc_ms = at(5, 18);
        assert!(!range_dates(Some(&range)).2);
        range.range_started_utc_ms = at(1, 12);
        range.horizon_ended_utc_ms = at(8, 12);
        range.range_ended_utc_ms = at(8, 12);
        assert_eq!(
            range_dates(Some(&range)),
            ("2026-09-01".into(), "2026-09-08".into(), false)
        );
        assert!(range_dates(None).2);
    }

    #[test]
    fn keyboard_has_93_distinct_keys_that_do_not_overlap() {
        assert_eq!(KEYBOARD.len(), 93);
        let codes = KEYBOARD
            .iter()
            .map(|(code, ..)| *code)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(codes.len(), 93);
        for (index, &(_, a, ax, ay, aw, ah)) in KEYBOARD.iter().enumerate() {
            assert!(
                ax + aw <= 19.75 && ay + ah <= 6.5,
                "{a} is outside the board"
            );
            for &(_, b, bx, by, bw, bh) in &KEYBOARD[index + 1..] {
                let apart = ax + aw <= bx || bx + bw <= ax || ay + ah <= by || by + bh <= ay;
                assert!(apart, "{a} overlaps {b}");
            }
        }
    }
}
