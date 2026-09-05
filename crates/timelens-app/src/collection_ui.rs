use crate::{AppWindow, CollectionState, KeyCell, UiState};
use anyhow::{Context, Result, anyhow};
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
fn refresh(w: &AppWindow, d: &mut Data, s: &Storage) -> Result<()> {
    let g = w.global::<CollectionState>();
    d.apps = s.known_applications()?;
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
            "活动名单：{}\n输入名单：{}\n截图名单：{}",
            names(&p.activity),
            names(&p.input),
            names(&s.snapshot_exclusions()?.into_iter().collect())
        )
        .into(),
    );
    Ok(())
}
fn select(w: &AppWindow, d: &Data, s: &Storage, index: i32) -> Result<()> {
    let (id, _) = d.apps.get(index as usize).context("请选择应用")?;
    let p = s.collection_policy()?;
    let g = w.global::<CollectionState>();
    g.set_app_index(index);
    g.set_activity(p.activity.contains(id));
    g.set_input(p.input.contains(id));
    g.set_snapshots(s.snapshot_exclusions()?.contains(id));
    let (windows, reports, ai) = s.application_deletion_impact(id)?;
    g.set_impact(format!("此组有 {windows} 个窗口实例、{reports} 份本地报告和 {ai} 个引用它的 AI 任务。删除后不可撤销。" ).into());
    g.set_delete_confirmed(false);
    g.set_merge_confirmed(false);
    Ok(())
}
pub fn install(
    w: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    timeline: Rc<RefCell<UiState>>,
) -> Result<Timer> {
    let data = Rc::new(RefCell::new(Data::default()));
    let (sender, receiver) = mpsc::channel::<Result<Option<String>, String>>();
    w.global::<CollectionState>()
        .set_date(chrono::Local::now().format("%Y-%m-%d").to_string().into());
    refresh(w, &mut data.borrow_mut(), &*lock(&storage)?)?;
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
                        .set_status(e.to_string().into());
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
                    .set_status(e.to_string().into());
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
                g.set_status(e.to_string().into());
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
                let _ai = if action == 5 {
                    Some(crate::ai::pause_and_wait()?)
                } else {
                    None
                };
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
                        }
                        4 => {
                            let (a, b) = link.context("请选择合并关联")?;
                            s.set_application_merge(&a, &b, false)?;
                        }
                        5 => {
                            if !confirmed {
                                return Err(anyhow!("请确认删除影响"));
                            }
                            s.delete_application_history(&id.context("请选择应用")?)?;
                            crate::ai_ui::dataset_changed();
                        }
                        _ => return Err(anyhow!("未知操作")),
                    }
                    Ok(None)
                })
            })();
            let _ = sender.send(result.map_err(|e| e.to_string()));
        });
    });
    let weak = w.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(w) = weak.upgrade() else {
            return;
        };
        let g = w.global::<CollectionState>();
        while let Ok(result) = receiver.try_recv() {
            g.set_busy(false);
            match result {
                Err(e) => g.set_status(e.into()),
                Ok(added) => {
                    if let Ok(s) = storage.try_lock() {
                        let mut d = data.borrow_mut();
                        let _ = refresh(&w, &mut d, &s);
                        if let Some(id) = added {
                            if !d.apps.iter().any(|(i, _)| i == &id) {
                                let name = std::path::Path::new(id.trim_start_matches("path:"))
                                    .file_name()
                                    .map_or(id.clone(), |s| s.to_string_lossy().into_owned());
                                d.apps.push((id.clone(), name));
                                g.set_apps(strings(d.apps.iter().map(|(_, n)| n.clone())));
                            }
                            let index = d.apps.iter().position(|(i, _)| i == &id).unwrap_or(0);
                            let _ = select(&w, &d, &s, index as i32);
                        } else {
                            g.set_status(
                                "已保存，采集规则在下一次检查时生效（通常不超过 1 秒）".into(),
                            );
                            g.set_delete_confirmed(false);
                            g.set_merge_confirmed(false);
                        }
                    }
                }
            }
        }
    });
    Ok(timer)
}
fn statistics(w: &AppWindow, s: &Storage, range: &UiState) -> Result<()> {
    let g = w.global::<CollectionState>();
    let date = g.get_date();
    chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")?;
    let frequencies: std::collections::BTreeMap<_, _> =
        s.physical_key_frequencies(&date)?.into_iter().collect();
    let max = frequencies.values().copied().max().unwrap_or(1).max(1) as f32;
    let rows: [&[(u32, &str)]; 5] = [
        &[
            (1, "Esc"),
            (2, "1"),
            (3, "2"),
            (4, "3"),
            (5, "4"),
            (6, "5"),
            (7, "6"),
            (8, "7"),
            (9, "8"),
            (10, "9"),
            (11, "0"),
            (12, "−"),
            (13, "="),
            (14, "⌫"),
        ],
        &[
            (15, "Tab"),
            (16, "Q"),
            (17, "W"),
            (18, "E"),
            (19, "R"),
            (20, "T"),
            (21, "Y"),
            (22, "U"),
            (23, "I"),
            (24, "O"),
            (25, "P"),
            (26, "["),
            (27, "]"),
            (43, "\\"),
        ],
        &[
            (58, "Caps"),
            (30, "A"),
            (31, "S"),
            (32, "D"),
            (33, "F"),
            (34, "G"),
            (35, "H"),
            (36, "J"),
            (37, "K"),
            (38, "L"),
            (39, ";"),
            (40, "'"),
            (28, "Enter"),
        ],
        &[
            (42, "LShift"),
            (44, "Z"),
            (45, "X"),
            (46, "C"),
            (47, "V"),
            (48, "B"),
            (49, "N"),
            (50, "M"),
            (51, ","),
            (52, "."),
            (53, "/"),
            (54, "RShift"),
        ],
        &[
            (29, "LCtrl"),
            (0x15b, "LWin"),
            (56, "LAlt"),
            (57, "Space"),
            (0x138, "RAlt"),
            (0x15c, "RWin"),
            (0x11d, "RCtrl"),
            (0x148, "↑"),
            (0x14b, "←"),
            (0x150, "↓"),
            (0x14d, "→"),
            (511, "其他"),
        ],
    ];
    let mut keys = vec![];
    for (row, items) in rows.iter().enumerate() {
        for (col, (code, label)) in items.iter().enumerate() {
            let count = frequencies.get(code).copied().unwrap_or(0);
            keys.push(KeyCell {
                label: (*label).into(),
                count: count.to_string().into(),
                strength: (count as f32 / max).sqrt(),
                col: col as f32 * 16.0 / items.len() as f32,
                row: row as f32,
                units: 16.0 / items.len() as f32,
            });
        }
    }
    g.set_keys(Rc::new(VecModel::from(keys)).into());
    let totals = s.permanent_input_totals()?;
    g.set_totals(
        format!(
            "永久累计 · 键盘 {} · 左键 {} · 中键 {} · 右键 {}",
            totals[0], totals[1], totals[2], totals[3]
        )
        .into(),
    );
    let totals = s.system_totals(range.range_started_utc_ms, range.range_ended_utc_ms)?;
    g.set_system(if totals.is_empty() {
        "此范围没有系统区间记录".into()
    } else {
        totals
            .into_iter()
            .map(|(kind, n)| format!("{}：{}", system_label(&kind), crate::format_duration(n)))
            .collect::<Vec<_>>()
            .join("\n")
            .into()
    });
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
        "clock_discontinuity" => "时钟不连续",
        "system_end" => "系统正常结束",
        _ => kind,
    }
}
