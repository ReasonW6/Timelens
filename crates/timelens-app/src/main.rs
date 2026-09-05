#![cfg(windows)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai;
mod ai_ui;
mod collection_ui;
mod data_ui;
mod local_config;
mod recovery;
mod snapshot;
mod tray;

use std::{
    cell::RefCell,
    env,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel};
use timelens_ipc::{
    COLLECTOR_RESET_PAUSED_FILE, COLLECTOR_RESET_REQUEST_FILE, PROTOCOL_VERSION,
    SingleInstanceGuard, current_pipe_name, run_server_collector_message, run_server_probe,
};
use timelens_storage::{
    LocalReport, RetentionPolicy, SnapshotMissingReason, SnapshotPolicy, SnapshotSlot, Storage,
    TimelineApplication, TimelineSnapshot,
};

const COLLECTOR_NAMES: &[&str] = &["timelens-collector.exe", "Timelens.Collector.exe"];

slint::slint! {
    import { Button, CheckBox, ListView } from "std-widgets.slint";
    import { AiPanel, AiState } from "ui/ai-panel.slint";
    import { DataPanel, DataState } from "ui/data-panel.slint";
    export { DataState } from "ui/data-panel.slint";
    import { CollectionPanel, CollectionState, KeyCell } from "ui/collection-panel.slint";
    export { CollectionState, KeyCell } from "ui/collection-panel.slint";
    export { AiState } from "ui/ai-panel.slint";

    export struct AppRow {
        name: string,
        summary: string,
        open-ratio: float,
        display-ratio: float,
        focus-ratio: float,
    }

    export struct WindowRow {
        label: string,
        summary: string,
    }

    export struct SegmentRow {
        offset: float,
        width: float,
        kind: int,
    }

    export struct SnapshotRow {
        time: string,
        display: string,
        result: string,
    }

    export struct ReportAppRow {
        name: string,
        summary: string,
    }

    component FlatButton inherits Rectangle {
        in property <string> text;
        in property <bool> danger: false;
        callback clicked;
        height: 34px;
        border-radius: 8px;
        background: touch.pressed ? (root.danger ? #7b3039 : #234d72)
                                  : (root.danger ? #4b252c : #1a3147);
        border-width: 1px;
        border-color: root.danger ? #8c4652 : #2b506e;
        Text {
            text: root.text;
            color: root.danger ? #ffc1c8 : #d8eaff;
            horizontal-alignment: center;
            vertical-alignment: center;
            font-size: 12px;
        }
        touch := TouchArea { clicked => { root.clicked(); } }
    }

    component MetricCard inherits Rectangle {
        in property <string> label;
        in property <string> value;
        background: #151f2b;
        border-radius: 10px;
        VerticalLayout {
            padding: 12px;
            spacing: 4px;
            Text { text: root.label; color: #7e92a8; font-size: 11px; }
            Text { text: root.value; color: #eef5fb; font-size: 17px; font-weight: 600; }
        }
    }

    export component AppWindow inherits Window {
        title: "Timelens";
        width: 1120px;
        height: 720px;
        background: rgb(11, 17, 24);

        in property <string> storage-status;
        in property <string> collector-status;
        in property <string> data-path;
        in property <string> range-label;
        in property <string> selected-name: "尚无应用数据";
        in property <string> opened-value: "0 秒";
        in property <string> displayed-value: "0 秒";
        in property <string> focused-value: "0 秒";
        in property <string> background-value: "0 秒";
        in property <string> input-value: "键盘 0 · 鼠标 0";
        in property <string> coverage-value: "未记录清理或中断";
        in property <string> retention-value: "30 天 · 100 MiB";
        in property <string> action-status: "";
        in property <[AppRow]> apps;
        in property <[WindowRow]> windows;
        in property <[SegmentRow]> segments;
        in property <[SnapshotRow]> snapshots;
        in property <image> snapshot-preview;
        in property <string> snapshot-detail: "请选择一条快照记录";
        in property <string> snapshot-policy-label: "快照已关闭";
        in property <string> snapshot-exclusion-label: "先在主界面选择应用";
        in property <[ReportAppRow]> report-apps;
        in property <string> report-summary: "正在生成本地报告…";
        in-out property <int> selected-index: -1;
        in-out property <int> snapshot-selected-index: -1;
        in-out property <bool> windows-expanded: false;
        in-out property <bool> snapshot-open: false;
        in-out property <bool> report-open: false;
        private property <bool> clear-confirm-open: false;
        in-out property <float> selection-left: 0;
        in-out property <float> selection-right: 1;
        private property <bool> middle-dragging: false;
        private property <float> middle-start: 0;

        callback app-selected(int);
        callback horizon-selected(int);
        callback range-selected(float, float);
        callback retention-selected(int);
        callback run-cleanup;
        callback clear-all;
        callback snapshot-opened;
        callback snapshot-row-selected(int);
        callback snapshot-delete-selected;
        callback snapshot-capture-now;
        callback snapshot-toggle-enabled;
        callback snapshot-interval-selected(int);
        callback snapshot-toggle-target;
        callback snapshot-retention-selected(int, int);
        callback snapshot-toggle-exclusion;
        callback report-generate;

        VerticalLayout {
            padding: 22px;
            spacing: 14px;

            HorizontalLayout {
                height: 54px;
                VerticalLayout {
                    spacing: 2px;
                    Text { text: "TIMELENS"; color: #eef6ff; font-size: 25px; font-weight: 700; }
                    Text { text: root.collector-status; color: #6f91ae; font-size: 11px; }
                }
                Rectangle { horizontal-stretch: 1; }
                FlatButton { width: 68px; text: "AI 总结"; clicked => { AiState.open = true; AiState.opened(); } }
                FlatButton { width: 78px; text: "数据备份"; clicked => { DataState.open = true; } }
                FlatButton { width: 78px; text: "采集统计"; clicked => { CollectionState.open = true; CollectionState.opened(); } }
                VerticalLayout {
                    alignment: end;
                    Text { text: root.range-label; color: #cfe8ff; font-size: 13px; horizontal-alignment: right; }
                    Text { text: root.coverage-value; color: #70d6a1; font-size: 11px; horizontal-alignment: right; }
                }
            }

            Rectangle {
                height: 122px;
                background: #111b26;
                border-radius: 12px;
                VerticalLayout {
                    padding: 14px;
                    spacing: 8px;
                    HorizontalLayout {
                        Text { text: "时间范围"; color: #a8bacb; font-size: 12px; }
                        Rectangle { horizontal-stretch: 1; }
                        FlatButton { width: 58px; text: "6 小时"; clicked => { root.horizon-selected(6); } }
                        FlatButton { width: 62px; text: "24 小时"; clicked => { root.horizon-selected(24); } }
                        FlatButton { width: 54px; text: "7 天"; clicked => { root.horizon-selected(168); } }
                    }
                    Text { text: "在下方时间带按住鼠标中键拖动可截取区间"; color: rgb(97, 121, 142); font-size: 10px; }
                    axis := Rectangle {
                        height: 42px;
                        background: rgb(11, 19, 28);
                        border-radius: 8px;
                        for segment in root.segments : Rectangle {
                            x: segment.offset * parent.width;
                            width: max(2px, segment.width * parent.width);
                            height: parent.height;
                            background: segment.kind == 2 ? #62b6ff55
                                      : segment.kind == 1 ? #54d7a255
                                      : #a58bff3c;
                        }
                        Rectangle {
                            x: min(root.selection-left, root.selection-right) * parent.width;
                            width: abs(root.selection-right - root.selection-left) * parent.width;
                            height: parent.height;
                            background: #8cc8ff24;
                            border-width: 1px;
                            border-color: #8cc8ff;
                        }
                        axis-touch := TouchArea {
                            pointer-event(event) => {
                                if (event.button == PointerEventButton.middle && event.kind == PointerEventKind.down) {
                                    root.middle-dragging = true;
                                    root.middle-start = axis-touch.mouse-x / axis-touch.width;
                                    root.selection-left = root.middle-start;
                                    root.selection-right = root.middle-start;
                                }
                                if (event.button == PointerEventButton.middle && event.kind == PointerEventKind.up && root.middle-dragging) {
                                    root.middle-dragging = false;
                                    root.selection-right = axis-touch.mouse-x / axis-touch.width;
                                    root.range-selected(root.middle-start, root.selection-right);
                                }
                            }
                            moved => {
                                if (root.middle-dragging) {
                                    root.selection-right = axis-touch.mouse-x / axis-touch.width;
                                }
                            }
                        }
                    }
                }
            }

            HorizontalLayout {
                spacing: 14px;
                vertical-stretch: 1;

                Rectangle {
                    width: 330px;
                    background: #101923;
                    border-radius: 12px;
                    VerticalLayout {
                        padding: 12px;
                        spacing: 8px;
                        Text { text: "选中时段内的应用"; color: #94a9bc; font-size: 12px; }
                        ListView {
                            for app[index] in root.apps : Rectangle {
                                height: 70px;
                                border-radius: 9px;
                                background: index == root.selected-index ? #1b3650 : app-row-touch.has-hover ? #152535 : #111c27;
                                VerticalLayout {
                                    padding: 10px;
                                    spacing: 5px;
                                    HorizontalLayout {
                                        Text { text: app.name; color: #edf6ff; font-size: 13px; font-weight: 600; overflow: elide; }
                                        Rectangle { horizontal-stretch: 1; }
                                        Text { text: app.summary; color: #7890a6; font-size: 10px; }
                                    }
                                    Rectangle {
                                        height: 7px;
                                        background: #0a1118;
                                        border-radius: 4px;
                                        Rectangle { width: app.open-ratio * parent.width; height: parent.height; background: #7c6fc766; border-radius: 4px; }
                                        Rectangle { width: app.display-ratio * parent.width; height: parent.height; background: #4ac58a99; border-radius: 4px; }
                                        Rectangle { width: app.focus-ratio * parent.width; height: parent.height; background: #5caeff; border-radius: 4px; }
                                    }
                                }
                                app-row-touch := TouchArea { clicked => { root.app-selected(index); } }
                            }
                        }
                    }
                }

                Rectangle {
                    horizontal-stretch: 1;
                    background: #101923;
                    border-radius: 12px;
                    VerticalLayout {
                        padding: 16px;
                        spacing: 12px;
                        HorizontalLayout {
                            VerticalLayout {
                                Text { text: root.selected-name; color: #f2f7fc; font-size: 21px; font-weight: 650; overflow: elide; }
                                Text { text: root.input-value; color: #6f8da6; font-size: 11px; }
                            }
                            Rectangle { horizontal-stretch: 1; }
                            FlatButton {
                                width: 108px;
                                text: root.windows-expanded ? "收起窗口" : "展开窗口";
                                clicked => { root.windows-expanded = !root.windows-expanded; }
                            }
                        }
                        HorizontalLayout {
                            spacing: 8px;
                            MetricCard { label: "打开"; value: root.opened-value; }
                            MetricCard { label: "显示中"; value: root.displayed-value; }
                            MetricCard { label: "聚焦中"; value: root.focused-value; }
                            MetricCard { label: "后台"; value: root.background-value; }
                        }
                        Rectangle {
                            height: 54px;
                            background: rgb(11, 19, 28);
                            border-radius: 8px;
                            for segment in root.segments : Rectangle {
                                x: segment.offset * parent.width;
                                width: max(2px, segment.width * parent.width);
                                height: segment.kind == 2 ? 16px : segment.kind == 1 ? 14px : 12px;
                                y: segment.kind == 2 ? 5px : segment.kind == 1 ? 21px : 37px;
                                background: segment.kind == 2 ? #62b6ff : segment.kind == 1 ? #54d7a2 : #9a82df;
                                border-radius: 3px;
                            }
                        }
                        if root.windows-expanded : ListView {
                            for item in root.windows : Rectangle {
                                height: 48px;
                                background: #121e2a;
                                border-radius: 7px;
                                HorizontalLayout {
                                    padding: 10px;
                                    Text { text: item.label; color: #cfe1ef; font-size: 12px; }
                                    Rectangle { horizontal-stretch: 1; }
                                    Text { text: item.summary; color: #6f879c; font-size: 10px; }
                                }
                            }
                        }
                        Rectangle { vertical-stretch: 1; }
                    }
                }
            }

            Rectangle {
                height: 74px;
                background: #101923;
                border-radius: 11px;
                HorizontalLayout {
                    padding: 12px;
                    spacing: 8px;
                    VerticalLayout {
                        Text { text: "数据保留  " + root.retention-value; color: #a7bacb; font-size: 12px; }
                        Text { text: root.storage-status + "  ·  " + root.data-path; color: #536d83; font-size: 10px; overflow: elide; }
                        Text { text: root.action-status; color: #77cda1; font-size: 10px; }
                    }
                    Rectangle { horizontal-stretch: 1; }
                    FlatButton { width: 70px; text: "快照"; clicked => { root.snapshot-open = true; root.snapshot-opened(); } }
                    FlatButton { width: 84px; text: "时段报告"; clicked => { root.report-open = true; root.report-generate(); } }
                    FlatButton { width: 50px; text: "7 天"; clicked => { root.retention-selected(7); } }
                    FlatButton { width: 56px; text: "30 天"; clicked => { root.retention-selected(30); } }
                    FlatButton { width: 56px; text: "90 天"; clicked => { root.retention-selected(90); } }
                    FlatButton { width: 54px; text: "永久"; clicked => { root.retention-selected(0); } }
                    FlatButton { width: 78px; text: "立即清理"; clicked => { root.run-cleanup(); } }
                    FlatButton { width: 86px; text: "清空全部"; danger: true; clicked => { root.clear-confirm-open = true; } }
                }
            }
        }

        if root.clear-confirm-open : Rectangle {
            z: 100;
            width: parent.width;
            height: parent.height;
            background: #05080de6;
            TouchArea { clicked => { } }
            Rectangle {
                width: min(560px, parent.width - 48px);
                height: 320px;
                x: (parent.width - self.width) / 2;
                y: (parent.height - self.height) / 2;
                background: #111d29;
                border-width: 1px;
                border-color: #8c4652;
                border-radius: 12px;
                VerticalLayout {
                    padding: 24px;
                    spacing: 14px;
                    Text { text: "确认清空全部本地记录？"; color: #ffc1c8; font-size: 21px; font-weight: 600; }
                    Text { text: "将删除活动、输入、快照、本地报告、AI 总结与对话，并轮换数据密钥。此操作无法撤销。"; color: #d8eaff; font-size: 13px; wrap: word-wrap; }
                    Text { text: "普通设置与排除规则保留。外部备份和导出文件不受影响；如需保留记录，请先取消并导出备份。"; color: #91a8bb; font-size: 12px; wrap: word-wrap; }
                    Text { text: "数据位置：" + root.data-path; color: #91a8bb; font-size: 11px; wrap: word-wrap; }
                    CheckBox { text: "同时删除当前提供商的 AI 凭据"; checked <=> DataState.clear-credentials; }
                    HorizontalLayout {
                        spacing: 12px;
                        Button { text: "取消，保留记录"; clicked => { root.clear-confirm-open = false; } }
                        Button { text: "确认清空全部记录"; clicked => { root.clear-confirm-open = false; root.clear-all(); } }
                    }
                }
            }
        }

        if root.snapshot-open : Rectangle {
            z: 10;
            width: parent.width;
            height: parent.height;
            background: #05080dcf;
            Rectangle {
                width: min(980px, parent.width - 48px);
                height: min(620px, parent.height - 48px);
                x: (parent.width - self.width) / 2;
                y: (parent.height - self.height) / 2;
                background: #101923;
                border-width: 1px;
                border-color: #2b506e;
                border-radius: 14px;
                VerticalLayout {
                    padding: 16px;
                    spacing: 10px;
                    HorizontalLayout {
                        Text { text: "定时屏幕快照"; color: #eef6ff; font-size: 20px; font-weight: 650; }
                        Rectangle { horizontal-stretch: 1; }
                        Text { text: root.snapshot-policy-label; color: #79a9ca; font-size: 11px; vertical-alignment: center; }
                        FlatButton { width: 62px; text: "关闭"; clicked => { root.snapshot-open = false; } }
                    }
                    HorizontalLayout {
                        spacing: 6px;
                        FlatButton { width: 76px; text: "启用/停用"; clicked => { root.snapshot-toggle-enabled(); } }
                        FlatButton { width: 70px; text: "当前/全部"; clicked => { root.snapshot-toggle-target(); } }
                        Text { text: "间隔"; color: #70879b; font-size: 11px; vertical-alignment: center; }
                        FlatButton { width: 38px; text: "1"; clicked => { root.snapshot-interval-selected(1); } }
                        FlatButton { width: 38px; text: "3"; clicked => { root.snapshot-interval-selected(3); } }
                        FlatButton { width: 38px; text: "5"; clicked => { root.snapshot-interval-selected(5); } }
                        FlatButton { width: 42px; text: "10"; clicked => { root.snapshot-interval-selected(10); } }
                        FlatButton { width: 42px; text: "15"; clicked => { root.snapshot-interval-selected(15); } }
                        FlatButton { width: 42px; text: "30"; clicked => { root.snapshot-interval-selected(30); } }
                        FlatButton { width: 42px; text: "60"; clicked => { root.snapshot-interval-selected(60); } }
                        Rectangle { horizontal-stretch: 1; }
                        FlatButton { width: 86px; text: "立即拍一张"; clicked => { root.snapshot-capture-now(); } }
                    }
                    HorizontalLayout {
                        spacing: 6px;
                        Text { text: "独立保留"; color: #70879b; font-size: 11px; vertical-alignment: center; }
                        FlatButton { width: 48px; text: "1天"; clicked => { root.snapshot-retention-selected(1, 1024); } }
                        FlatButton { width: 48px; text: "3天"; clicked => { root.snapshot-retention-selected(3, 1024); } }
                        FlatButton { width: 48px; text: "7天"; clicked => { root.snapshot-retention-selected(7, 1024); } }
                        FlatButton { width: 52px; text: "30天"; clicked => { root.snapshot-retention-selected(30, 1024); } }
                        FlatButton { width: 52px; text: "90天"; clicked => { root.snapshot-retention-selected(90, 1024); } }
                        FlatButton { width: 60px; text: "365天"; clicked => { root.snapshot-retention-selected(365, 1024); } }
                        FlatButton { width: 52px; text: "永久"; clicked => { root.snapshot-retention-selected(0, 1024); } }
                        Text { text: "上限 1 GiB"; color: #526b80; font-size: 10px; vertical-alignment: center; }
                        Rectangle { horizontal-stretch: 1; }
                        Text { text: root.snapshot-exclusion-label; color: #6f8da6; font-size: 10px; vertical-alignment: center; overflow: elide; }
                        FlatButton { width: 118px; text: "切换所选应用排除"; clicked => { root.snapshot-toggle-exclusion(); } }
                    }
                    HorizontalLayout {
                        spacing: 10px;
                        Rectangle {
                            width: 310px;
                            background: rgb(11, 19, 28);
                            border-radius: 9px;
                            VerticalLayout {
                                padding: 7px;
                                Text { text: "当前时段快照与明确缺失"; color: #8fa6b9; font-size: 11px; }
                                ListView {
                                    for item[index] in root.snapshots : Rectangle {
                                        height: 54px;
                                        border-radius: 7px;
                                        background: index == root.snapshot-selected-index ? #1b3650 : snapshot-row-touch.has-hover ? #152535 : #101b26;
                                        VerticalLayout {
                                            padding: 7px;
                                            Text { text: item.time + "  ·  " + item.display; color: #dcecf8; font-size: 11px; overflow: elide; }
                                            Text { text: item.result; color: #6f91ae; font-size: 10px; overflow: elide; }
                                        }
                                        snapshot-row-touch := TouchArea { clicked => { root.snapshot-row-selected(index); } }
                                    }
                                }
                            }
                        }
                        Rectangle {
                            horizontal-stretch: 1;
                            background: #091019;
                            border-radius: 9px;
                            VerticalLayout {
                                padding: 10px;
                                spacing: 8px;
                                Image { source: root.snapshot-preview; image-fit: contain; vertical-stretch: 1; }
                                Text { text: root.snapshot-detail; color: #7f98ad; font-size: 10px; horizontal-alignment: center; overflow: elide; }
                                HorizontalLayout {
                                    Rectangle { horizontal-stretch: 1; }
                                    FlatButton { width: 92px; text: "删除所选快照"; danger: true; clicked => { root.snapshot-delete-selected(); } }
                                }
                            }
                        }
                    }
                }
            }
        }

        if root.report-open : Rectangle {
            z: 11;
            width: parent.width;
            height: parent.height;
            background: #05080dd8;
            Rectangle {
                width: min(820px, parent.width - 64px);
                height: min(560px, parent.height - 64px);
                x: (parent.width - self.width) / 2;
                y: (parent.height - self.height) / 2;
                background: #101923;
                border-width: 1px;
                border-color: #2b506e;
                border-radius: 14px;
                VerticalLayout {
                    padding: 18px;
                    spacing: 12px;
                    HorizontalLayout {
                        Text { text: "可复现的本地时段报告"; color: #eef6ff; font-size: 20px; font-weight: 650; }
                        Rectangle { horizontal-stretch: 1; }
                        FlatButton { width: 64px; text: "重新生成"; clicked => { root.report-generate(); } }
                        FlatButton { width: 58px; text: "关闭"; clicked => { root.report-open = false; } }
                    }
                    Text { text: root.report-summary; color: rgb(142, 171, 193); font-size: 12px; wrap: word-wrap; }
                    Rectangle {
                        vertical-stretch: 1;
                        background: rgb(11, 19, 28);
                        border-radius: 9px;
                        ListView {
                            for app in root.report-apps : Rectangle {
                                height: 54px;
                                background: #101b26;
                                HorizontalLayout {
                                    padding: 9px;
                                    Text { text: app.name; color: #e1effa; font-size: 12px; overflow: elide; }
                                    Rectangle { horizontal-stretch: 1; }
                                    Text { text: app.summary; color: #6f8da6; font-size: 10px; }
                                }
                            }
                        }
                    }
                    Text { text: "报告只固化聚合结果、覆盖率、缺失原因与规则版本，不复制原始分钟事件。"; color: #526b80; font-size: 10px; }
                }
            }
        }
        if AiState.open : AiPanel { x: parent.width - self.width; y: 0; width: min(560px, parent.width - 24px); height: parent.height; }
        if DataState.open : DataPanel { x: parent.width - self.width; y: 0; width: min(560px, parent.width - 24px); height: parent.height; }
        if CollectionState.open : CollectionPanel { x: parent.width - self.width; y: 0; width: min(560px, parent.width - 24px); height: parent.height; }
    }
}

fn main() -> Result<()> {
    let options = Options::parse(env::args_os().skip(1))?;
    timelens_ai::credentials::require_ordinary_privilege().map_err(anyhow::Error::msg)?;
    if options.shutdown {
        tray::activate_existing(true);
        return Ok(());
    }
    if options.uninstall_data {
        let _instance = SingleInstanceGuard::acquire_core()?;
        let _collector =
            SingleInstanceGuard::acquire_collector().context("删除数据前必须先停止采集器")?;
        let data = options.data_directory()?;
        let control = options.control_directory()?;
        if options.data_directory.is_some() {
            for id in Storage::dataset_credential_ids(&data)? {
                timelens_ai::credentials::delete(&id).map_err(anyhow::Error::msg)?;
            }
        } else {
            timelens_ai::credentials::delete_all().map_err(anyhow::Error::msg)?;
        }
        Storage::delete_owned_dataset(&data)?;
        if data != control {
            Storage::delete_owned_dataset(&control)?;
        }
        local_config::delete_pointer(&control)?;
        return Ok(());
    }
    let waiting_since = std::time::Instant::now();
    let _instance = loop {
        match SingleInstanceGuard::acquire_core() {
            Ok(instance) => break instance,
            Err(_) if options.recover && waiting_since.elapsed() < Duration::from_secs(30) => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                if tray::activate_existing(false) {
                    return Ok(());
                }
                return Err(error)
                    .context("Timelens is already running, but its window is not ready");
            }
        }
    };
    let control_directory = options.control_directory()?;
    // The singleton proves no previous core maintenance operation remains alive.
    match std::fs::remove_file(control_directory.join(COLLECTOR_RESET_REQUEST_FILE)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut storage = recovery::open(
        options.data_directory(),
        &control_directory,
        options.recover,
    )?;
    storage.set_control_directory(&control_directory)?;
    let data_directory = storage.data_directory().to_path_buf();
    let retention = storage
        .apply_retention(unix_time_ms())
        .context("failed to apply the configured retention policy")?;
    if retention.activity_items > 0
        || retention.input_items > 0
        || retention.snapshot_items > 0
        || retention.report_items > 0
    {
        println!(
            "retention cleanup complete: snapshot_items={} report_items={} activity_items={} input_items={} released_bytes={}",
            retention.snapshot_items,
            retention.report_items,
            retention.activity_items,
            retention.input_items,
            retention.released_bytes
        );
    }
    storage.record_component_health("core", PROTOCOL_VERSION, None)?;
    let pipe_name = options.pipe_name.unwrap_or(current_pipe_name()?);

    if options.handshake_once {
        let report = run_server_probe(&pipe_name, COLLECTOR_NAMES)?;
        storage.record_component_health(
            "collector",
            PROTOCOL_VERSION,
            Some(report.peer_process_id),
        )?;
        println!(
            "collector handshake ok: pid={} session={} path={} verification={:?}",
            report.peer_process_id,
            report.peer_session_id,
            report.peer_path.display(),
            report.verification
        );
        return Ok(());
    }

    let storage = Arc::new(Mutex::new(storage));
    if options.snapshot_once {
        let summary = snapshot::capture_now(&storage, &data_directory)?;
        println!("snapshot capture complete: {}", summary.label());
        return Ok(());
    }
    snapshot::spawn_scheduler(Arc::clone(&storage), data_directory.clone());
    let ai_service = ai::spawn(Arc::clone(&storage));
    let maintenance_storage = Arc::clone(&storage);
    thread::spawn(move || {
        loop {
            let _ = snapshot::run_heavy_task(|| {
                let s = maintenance_storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
                s.periodic_integrity_check(unix_time_ms())?;
                s.apply_retention(unix_time_ms())?;
                Ok(())
            });
            thread::sleep(Duration::from_secs(60));
        }
    });

    run_window(
        storage,
        data_directory,
        pipe_name,
        ai_service,
        options.background,
    )
}

enum CollectorStatus {
    Connected(String),
    Failed(String),
}

struct ActionStatus {
    message: String,
    refresh: bool,
    report: Option<std::result::Result<i64, String>>,
}

#[derive(Default)]
struct SnapshotUiState {
    slots: Vec<SnapshotSlot>,
    selected_slot_id: Option<i64>,
}

struct UiState {
    horizon_started_utc_ms: i64,
    horizon_ended_utc_ms: i64,
    range_started_utc_ms: i64,
    range_ended_utc_ms: i64,
    selected_identity: Option<String>,
    application_identities: Vec<String>,
    follow_now: bool,
}

impl UiState {
    fn recent_hours(hours: i64) -> Self {
        let ended = unix_time_ms();
        let started = ended.saturating_sub(hours * 3_600_000);
        Self {
            horizon_started_utc_ms: started,
            horizon_ended_utc_ms: ended,
            range_started_utc_ms: started,
            range_ended_utc_ms: ended,
            selected_identity: None,
            application_identities: Vec::new(),
            follow_now: true,
        }
    }

    fn select_fraction_range(&mut self, first: f32, second: f32) -> bool {
        let first = first.clamp(0.0, 1.0);
        let second = second.clamp(0.0, 1.0);
        let left = first.min(second);
        let right = first.max(second);
        if right - left < 0.002 {
            return false;
        }
        let span = self
            .horizon_ended_utc_ms
            .saturating_sub(self.horizon_started_utc_ms);
        self.range_started_utc_ms = self
            .horizon_started_utc_ms
            .saturating_add((span as f64 * f64::from(left)) as i64);
        self.range_ended_utc_ms = self
            .horizon_started_utc_ms
            .saturating_add((span as f64 * f64::from(right)) as i64);
        self.follow_now = false;
        true
    }
}

fn run_window(
    storage: Arc<Mutex<Storage>>,
    data_directory: PathBuf,
    pipe_name: String,
    ai_service: ai::AiService,
    background: bool,
) -> Result<()> {
    let window = AppWindow::new()?;
    let storage_status = {
        let storage = storage
            .lock()
            .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
        format!(
            "SQLCipher {} · schema v{}",
            storage.cipher_version(),
            storage.schema_version()?
        )
    };
    window.set_storage_status(storage_status.into());
    window.set_collector_status("等待受信采集器事件…".into());
    window.set_data_path(data_directory.display().to_string().into());

    let (status_sender, status_receiver) = mpsc::channel();
    let server_storage = Arc::clone(&storage);
    let server_data_directory = data_directory.clone();
    thread::spawn(move || {
        loop {
            let status = match run_server_collector_message(&pipe_name, COLLECTOR_NAMES, |batch| {
                ingest_if_not_resetting(&server_storage, &server_data_directory, batch)
            }) {
                Ok(report) => match server_storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))
                    .and_then(|storage| {
                        storage
                            .record_component_health(
                                "collector",
                                PROTOCOL_VERSION,
                                Some(report.peer_process_id),
                            )
                            .map_err(Into::into)
                    }) {
                    Ok(()) => CollectorStatus::Connected(format!(
                        "已验证采集器 · PID {} · {:?}",
                        report.peer_process_id, report.verification
                    )),
                    Err(error) => CollectorStatus::Failed(format!("数据库状态写入失败：{error}")),
                },
                Err(error) => CollectorStatus::Failed(error.to_string()),
            };
            if status_sender.send(status).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(250));
        }
    });

    let ui_state = Rc::new(RefCell::new(UiState::recent_hours(24)));
    let snapshot_ui_state = Rc::new(RefCell::new(SnapshotUiState::default()));
    let action_busy = Arc::new(AtomicBool::new(false));
    let (action_sender, action_receiver) = mpsc::channel::<ActionStatus>();

    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let action_busy = Arc::clone(&action_busy);
        window.on_app_selected(move |index| {
            if action_busy.load(Ordering::Acquire) || index < 0 {
                return;
            }
            let mut state = ui_state.borrow_mut();
            state.selected_identity = state.application_identities.get(index as usize).cloned();
            drop(state);
            if let Some(window) = weak.upgrade()
                && let Err(error) = refresh_timeline(&window, &storage, &ui_state)
            {
                window.set_action_status(format!("刷新失败：{error}").into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let action_busy = Arc::clone(&action_busy);
        window.on_horizon_selected(move |hours| {
            if action_busy.load(Ordering::Acquire) || hours <= 0 {
                return;
            }
            *ui_state.borrow_mut() = UiState::recent_hours(i64::from(hours));
            if let Some(window) = weak.upgrade()
                && let Err(error) = refresh_timeline(&window, &storage, &ui_state)
            {
                window.set_action_status(format!("刷新失败：{error}").into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let action_busy = Arc::clone(&action_busy);
        window.on_range_selected(move |left, right| {
            if action_busy.load(Ordering::Acquire) {
                return;
            }
            let mut state = ui_state.borrow_mut();
            if !state.select_fraction_range(left, right) {
                return;
            }
            drop(state);
            if let Some(window) = weak.upgrade()
                && let Err(error) = refresh_timeline(&window, &storage, &ui_state)
            {
                window.set_action_status(format!("刷新失败：{error}").into());
            }
        });
    }

    install_retention_callback(
        &window,
        Arc::clone(&storage),
        Arc::clone(&action_busy),
        action_sender.clone(),
    );
    install_cleanup_callback(
        &window,
        Arc::clone(&storage),
        Arc::clone(&action_busy),
        action_sender.clone(),
    );
    install_clear_callback(
        &window,
        Arc::clone(&storage),
        Arc::clone(&action_busy),
        action_sender.clone(),
    );
    install_snapshot_callbacks(
        &window,
        Arc::clone(&storage),
        data_directory.clone(),
        Rc::clone(&ui_state),
        Rc::clone(&snapshot_ui_state),
        Arc::clone(&action_busy),
        action_sender.clone(),
    );
    install_report_callback(
        &window,
        Arc::clone(&storage),
        Rc::clone(&ui_state),
        Arc::clone(&action_busy),
        action_sender,
    );

    refresh_timeline(&window, &storage, &ui_state)?;

    let _ai_timer = ai_ui::install(
        &window,
        Arc::clone(&storage),
        Rc::clone(&ui_state),
        ai_service,
    )?;
    let _data_timer = data_ui::install(
        &window,
        Arc::clone(&storage),
        Rc::clone(&ui_state),
        Rc::clone(&snapshot_ui_state),
    );
    let _tray_timer = tray::install(&window, Arc::clone(&storage))?;
    let _collection_timer =
        collection_ui::install(&window, Arc::clone(&storage), Rc::clone(&ui_state))?;

    let weak = window.as_weak();
    let timer_storage = Arc::clone(&storage);
    let timer_state = Rc::clone(&ui_state);
    let timer_snapshot_state = Rc::clone(&snapshot_ui_state);
    let timer_busy = Arc::clone(&action_busy);
    let last_refresh = Rc::new(RefCell::new(Instant::now()));
    let timer_last_refresh = Rc::clone(&last_refresh);
    let timer = Timer::default();
    let mut recovery_shown = false;
    let mut data_generation = ai_ui::data_generation();
    timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !recovery_shown && timer_storage.try_lock().is_ok_and(|s| s.is_quarantined()) {
            recovery_shown = true;
            window.global::<DataState>().set_open(true);
            window.global::<DataState>().set_recovery_needed(true);
            window.global::<DataState>().set_status(
                "发现数据损坏，已停止写入。请进入恢复界面，保留原件并恢复到新目录。".into(),
            );
        }
        while let Ok(status) = status_receiver.try_recv() {
            match status {
                CollectorStatus::Connected(message) => window.set_collector_status(message.into()),
                CollectorStatus::Failed(error) => {
                    if timer_storage
                        .lock()
                        .is_ok_and(|storage| collector_reset_active(storage.control_directory()))
                    {
                        window.set_collector_status("正在安全暂停采集器…".into());
                    } else {
                        window.set_collector_status(format!("握手失败：{error}").into());
                    }
                }
            }
        }
        let mut refresh_requested = false;
        if ai_ui::data_generation() != data_generation {
            data_generation = ai_ui::data_generation();
            *timer_snapshot_state.borrow_mut() = SnapshotUiState::default();
            window.set_snapshot_preview(Image::default());
            window.set_snapshot_selected_index(-1);
            window.set_snapshot_detail("数据已变更，请重新选择快照".into());
            window.set_report_apps(ModelRc::new(VecModel::<ReportAppRow>::default()));
            window.set_report_summary("数据已变更，请重新生成本地报告".into());
            window.global::<DataState>().set_clear_credentials(false);
            refresh_requested = true;
        }
        while let Ok(status) = action_receiver.try_recv() {
            window.set_action_status(status.message.into());
            refresh_requested |= status.refresh;
            if let Some(report) = status.report {
                match report {
                    Ok(report_id) => {
                        if let Err(error) = refresh_report(&window, &timer_storage, report_id) {
                            window.set_report_summary(format!("报告读取失败：{error}").into());
                        }
                    }
                    Err(error) => {
                        window.set_report_summary(format!("报告生成失败：{error}").into())
                    }
                }
            }
        }
        if !timer_busy.load(Ordering::Acquire)
            && (refresh_requested
                || timer_last_refresh.borrow().elapsed() >= Duration::from_secs(2))
        {
            match refresh_timeline(&window, &timer_storage, &timer_state).and_then(|_| {
                if window.get_snapshot_open() {
                    refresh_snapshot_panel(
                        &window,
                        &timer_storage,
                        &timer_state,
                        &timer_snapshot_state,
                    )?;
                }
                Ok(())
            }) {
                Ok(()) => *timer_last_refresh.borrow_mut() = Instant::now(),
                Err(error) => window.set_action_status(format!("刷新失败：{error}").into()),
            }
        }
    });
    if !background {
        window.show()?;
    }
    slint::run_event_loop_until_quit()?;
    drop(timer);
    Ok(())
}

fn ingest_if_not_resetting(
    storage: &Arc<Mutex<Storage>>,
    _data_directory: &Path,
    batch: &timelens_ipc::EventBatch,
) -> timelens_storage::Result<()> {
    let storage = storage.lock().map_err(|_| {
        timelens_storage::StorageError::Integrity("storage lock poisoned".to_owned())
    })?;
    if collector_reset_active(storage.control_directory()) {
        return Err(timelens_storage::StorageError::Integrity(
            "collector delivery is paused for clear all".to_owned(),
        ));
    }
    storage.ingest_event_batch(batch).map(|_| ())
}

fn collector_reset_active(data_directory: &Path) -> bool {
    data_directory.join(COLLECTOR_RESET_REQUEST_FILE).exists()
        || data_directory.join(COLLECTOR_RESET_PAUSED_FILE).exists()
}

fn refresh_timeline(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    ui_state: &Rc<RefCell<UiState>>,
) -> Result<()> {
    let (range_started, range_ended) = {
        let mut state = ui_state.borrow_mut();
        if state.follow_now {
            let horizon_duration = state
                .horizon_ended_utc_ms
                .saturating_sub(state.horizon_started_utc_ms);
            state.horizon_ended_utc_ms = unix_time_ms();
            state.horizon_started_utc_ms =
                state.horizon_ended_utc_ms.saturating_sub(horizon_duration);
            state.range_started_utc_ms = state.horizon_started_utc_ms;
            state.range_ended_utc_ms = state.horizon_ended_utc_ms;
        }
        (state.range_started_utc_ms, state.range_ended_utc_ms)
    };
    let (snapshot, policy) = {
        let storage = storage
            .lock()
            .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
        (
            storage.timeline_snapshot(range_started, range_ended)?,
            storage.retention_policy()?,
        )
    };
    render_snapshot(window, ui_state, snapshot, policy);
    Ok(())
}

fn render_snapshot(
    window: &AppWindow,
    ui_state: &Rc<RefCell<UiState>>,
    snapshot: TimelineSnapshot,
    policy: RetentionPolicy,
) {
    let range_ms = snapshot
        .range_ended_utc_ms
        .saturating_sub(snapshot.range_started_utc_ms)
        .max(1) as f64;
    let mut state = ui_state.borrow_mut();
    if state.selected_identity.as_ref().is_none_or(|selected| {
        !snapshot
            .applications
            .iter()
            .any(|application| &application.identity == selected)
    }) {
        state.selected_identity = snapshot
            .applications
            .first()
            .map(|application| application.identity.clone());
    }
    state.application_identities = snapshot
        .applications
        .iter()
        .map(|application| application.identity.clone())
        .collect();
    let selected_index = state
        .selected_identity
        .as_ref()
        .and_then(|identity| {
            snapshot
                .applications
                .iter()
                .position(|application| &application.identity == identity)
        })
        .map_or(-1, |index| index as i32);
    let horizon_span = state
        .horizon_ended_utc_ms
        .saturating_sub(state.horizon_started_utc_ms)
        .max(1) as f64;
    let selection_left = (state
        .range_started_utc_ms
        .saturating_sub(state.horizon_started_utc_ms) as f64
        / horizon_span)
        .clamp(0.0, 1.0) as f32;
    let selection_right = (state
        .range_ended_utc_ms
        .saturating_sub(state.horizon_started_utc_ms) as f64
        / horizon_span)
        .clamp(0.0, 1.0) as f32;
    drop(state);

    let app_rows = snapshot
        .applications
        .iter()
        .map(|application| AppRow {
            name: application.display_name.clone().into(),
            summary: format!(
                "{} · {} 个窗口",
                format_duration(application.opened_ms),
                application.window_count
            )
            .into(),
            open_ratio: ratio(application.opened_ms, range_ms),
            display_ratio: ratio(application.displayed_ms, range_ms),
            focus_ratio: ratio(application.focused_ms, range_ms),
        })
        .collect::<Vec<_>>();
    window.set_apps(ModelRc::new(VecModel::from(app_rows)));
    window.set_selected_index(selected_index);
    window.set_selection_left(selection_left);
    window.set_selection_right(selection_right);
    window.set_range_label(
        format!(
            "选中 {} · {} 个应用",
            format_duration(range_ms as u64),
            snapshot.applications.len()
        )
        .into(),
    );
    window.set_coverage_value(if snapshot.monitoring_gaps.is_empty() {
        "未记录清理或中断".into()
    } else {
        format!("存在 {} 个明确缺口", snapshot.monitoring_gaps.len()).into()
    });
    window.set_retention_value(retention_label(policy).into());

    let selected = (selected_index >= 0).then(|| &snapshot.applications[selected_index as usize]);
    render_application(window, selected, &snapshot);
}

fn render_application(
    window: &AppWindow,
    application: Option<&TimelineApplication>,
    snapshot: &TimelineSnapshot,
) {
    let Some(application) = application else {
        window.set_selected_name("尚无应用数据".into());
        window.set_opened_value("0 秒".into());
        window.set_displayed_value("0 秒".into());
        window.set_focused_value("0 秒".into());
        window.set_background_value("0 秒".into());
        window.set_input_value("键盘 0 · 鼠标 0".into());
        window.set_windows(ModelRc::new(VecModel::<WindowRow>::default()));
        window.set_segments(ModelRc::new(VecModel::<SegmentRow>::default()));
        return;
    };
    window.set_selected_name(application.display_name.clone().into());
    window.set_opened_value(format_duration(application.opened_ms).into());
    window.set_displayed_value(format_duration(application.displayed_ms).into());
    window.set_focused_value(format_duration(application.focused_ms).into());
    window.set_background_value(format_duration(application.background_ms).into());
    window.set_input_value(
        format!(
            "键盘 {} · 鼠标 {}",
            application.keyboard_count,
            application
                .left_click_count
                .saturating_add(application.middle_click_count)
                .saturating_add(application.right_click_count)
        )
        .into(),
    );
    let windows = application
        .windows
        .iter()
        .map(|item| WindowRow {
            label: format!("窗口 {}", item.number).into(),
            summary: format!(
                "打开 {} · 显示 {} · 聚焦 {} · 后台 {}",
                format_duration(item.opened_ms),
                format_duration(item.displayed_ms),
                format_duration(item.focused_ms),
                format_duration(item.background_ms)
            )
            .into(),
        })
        .collect::<Vec<_>>();
    window.set_windows(ModelRc::new(VecModel::from(windows)));

    let span = snapshot
        .range_ended_utc_ms
        .saturating_sub(snapshot.range_started_utc_ms)
        .max(1) as f64;
    let segments = application
        .segments
        .iter()
        .map(|segment| SegmentRow {
            offset: ((segment
                .started_utc_ms
                .saturating_sub(snapshot.range_started_utc_ms) as f64
                / span)
                .clamp(0.0, 1.0)) as f32,
            width: ((segment.ended_utc_ms.saturating_sub(segment.started_utc_ms) as f64 / span)
                .clamp(0.0, 1.0)) as f32,
            kind: if segment.focused {
                2
            } else if segment.displayed {
                1
            } else {
                0
            },
        })
        .collect::<Vec<_>>();
    window.set_segments(ModelRc::new(VecModel::from(segments)));
}

fn install_retention_callback(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    busy: Arc<AtomicBool>,
    sender: mpsc::Sender<ActionStatus>,
) {
    window.on_retention_selected(move |days| {
        if busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let storage = Arc::clone(&storage);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        thread::spawn(move || {
            let result = snapshot::run_heavy_task(|| -> Result<String> {
                let storage = storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
                let mut policy = storage.retention_policy()?;
                policy.days = (days > 0).then_some(days as u32);
                storage.set_retention_policy(policy)?;
                let report = storage.apply_retention(unix_time_ms())?;
                Ok(format!(
                    "保留策略已更新；清理快照 {} 项、报告 {} 项、活动 {} 项、输入 {} 项",
                    report.snapshot_items,
                    report.report_items,
                    report.activity_items,
                    report.input_items
                ))
            });
            busy.store(false, Ordering::Release);
            let _ = sender.send(ActionStatus {
                message: result.unwrap_or_else(|error| format!("保留策略失败：{error}")),
                refresh: true,
                report: None,
            });
        });
    });
}

fn install_cleanup_callback(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    busy: Arc<AtomicBool>,
    sender: mpsc::Sender<ActionStatus>,
) {
    window.on_run_cleanup(move || {
        if busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let storage = Arc::clone(&storage);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        thread::spawn(move || {
            let result = snapshot::run_heavy_task(|| {
                storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))
                    .and_then(|storage| storage.apply_retention(unix_time_ms()).map_err(Into::into))
            });
            busy.store(false, Ordering::Release);
            let message = match result {
                Ok(report) => format!(
                    "清理完成：快照 {} 项、报告 {} 项、活动 {} 项、输入 {} 项、释放 {}",
                    report.snapshot_items,
                    report.report_items,
                    report.activity_items,
                    report.input_items,
                    format_bytes(report.released_bytes)
                ),
                Err(error) => format!("清理失败：{error}"),
            };
            let _ = sender.send(ActionStatus {
                message,
                refresh: true,
                report: None,
            });
        });
    });
}

fn install_clear_callback(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    busy: Arc<AtomicBool>,
    sender: mpsc::Sender<ActionStatus>,
) {
    let weak = window.as_weak();
    window.on_clear_all(move || {
        if busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let storage = Arc::clone(&storage);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        let delete_credentials = weak
            .upgrade()
            .is_some_and(|w| w.global::<DataState>().get_clear_credentials());
        let _ = sender.send(ActionStatus {
            message: "正在暂停采集器并轮换全部数据密钥…".to_owned(),
            refresh: false,
            report: None,
        });
        thread::spawn(move || {
            let result = (|| -> Result<String> {
                let _ai = ai::pause_and_wait()?;
                snapshot::run_heavy_task(|| -> Result<String> {
                    let storage = storage
                        .lock()
                        .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
                    let report = storage.clear_all_coordinated(Duration::from_secs(12))?;
                    ai_ui::dataset_changed();
                    let mut credential_failures = 0;
                    if delete_credentials {
                        for mut profile in storage.ai_profiles()? {
                            if timelens_ai::credentials::delete(&profile.id).is_err() {
                                credential_failures += 1;
                            }
                            profile.tested_revision = None;
                            profile.credential_revision =
                                profile.credential_revision.saturating_add(1);
                            storage.save_ai_profile(&profile)?;
                        }
                    }
                    storage.record_component_health("core", PROTOCOL_VERSION, None)?;
                    Ok(format!(
                        "已清空 {} 行本地数据并轮换数据密钥{}",
                        report.deleted_rows,
                        if credential_failures > 0 {
                            format!("；有 {credential_failures} 项凭据未能删除，请检查提供商页")
                        } else if delete_credentials {
                            "；当前提供商凭据已删除".into()
                        } else {
                            String::new()
                        }
                    ))
                })
            })();
            busy.store(false, Ordering::Release);
            let _ = sender.send(ActionStatus {
                message: result.unwrap_or_else(|error| format!("清空失败：{error}")),
                refresh: true,
                report: None,
            });
        });
    });
}

fn install_snapshot_callbacks(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    data_directory: PathBuf,
    ui_state: Rc<RefCell<UiState>>,
    snapshot_state: Rc<RefCell<SnapshotUiState>>,
    busy: Arc<AtomicBool>,
    sender: mpsc::Sender<ActionStatus>,
) {
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_opened(move || {
            if let Some(window) = weak.upgrade()
                && let Err(error) =
                    refresh_snapshot_panel(&window, &storage, &ui_state, &snapshot_state)
            {
                window.set_action_status(format!("快照列表刷新失败：{error}").into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_row_selected(move |index| {
            if index < 0 {
                return;
            }
            let slot = snapshot_state.borrow().slots.get(index as usize).cloned();
            let Some(slot) = slot else {
                return;
            };
            snapshot_state.borrow_mut().selected_slot_id = Some(slot.id);
            let Some(window) = weak.upgrade() else {
                return;
            };
            window.set_snapshot_selected_index(index);
            if !slot.success {
                window.set_snapshot_preview(Image::default());
                window.set_snapshot_detail(
                    format!(
                        "该槽没有图片：{}",
                        slot.missing_reason
                            .map(snapshot_reason_label)
                            .unwrap_or("未知原因")
                    )
                    .into(),
                );
                return;
            }
            let result = snapshot::run_heavy_task(|| -> Result<(Image, String)> {
                let image = storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?
                    .load_snapshot_image(slot.id)?;
                let rgba = image::load_from_memory(&image.webp)?.to_rgba8();
                if rgba.dimensions() != (slot.pixel_width, slot.pixel_height) {
                    bail!("解密图片尺寸与元数据不一致");
                }
                let pixels = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                    rgba.as_raw(),
                    rgba.width(),
                    rgba.height(),
                );
                Ok((
                    Image::from_rgba8(pixels),
                    format!(
                        "{} × {} · {} · 解密与 SHA-256 完整性已验证",
                        slot.pixel_width,
                        slot.pixel_height,
                        format_bytes(slot.plaintext_bytes)
                    ),
                ))
            });
            match result {
                Ok((image, detail)) => {
                    window.set_snapshot_preview(image);
                    window.set_snapshot_detail(detail.into());
                }
                Err(error) => {
                    window.set_snapshot_preview(Image::default());
                    window.set_snapshot_detail(format!("预览失败：{error}").into());
                }
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_delete_selected(move || {
            let Some(slot_id) = snapshot_state.borrow().selected_slot_id else {
                return;
            };
            let result = storage
                .lock()
                .map_err(|_| anyhow::anyhow!("storage lock poisoned"))
                .and_then(|storage| storage.delete_snapshot(slot_id).map_err(Into::into));
            if let Some(window) = weak.upgrade() {
                match result {
                    Ok(true) => {
                        snapshot_state.borrow_mut().selected_slot_id = None;
                        window.set_snapshot_preview(Image::default());
                        window
                            .set_snapshot_detail("图片已删除；该槽保留为明确的用户删除记录".into());
                        window.set_action_status("已删除所选快照".into());
                        if let Err(error) =
                            refresh_snapshot_panel(&window, &storage, &ui_state, &snapshot_state)
                        {
                            window.set_action_status(format!("列表刷新失败：{error}").into());
                        }
                    }
                    Ok(false) => window.set_action_status("所选记录没有可删除的图片".into()),
                    Err(error) => window.set_action_status(format!("快照删除失败：{error}").into()),
                }
            }
        });
    }
    {
        let storage = Arc::clone(&storage);
        let data_directory = data_directory.clone();
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        window.on_snapshot_capture_now(move || {
            if busy.swap(true, Ordering::AcqRel) {
                return;
            }
            let storage = Arc::clone(&storage);
            let data_directory = data_directory.clone();
            let busy = Arc::clone(&busy);
            let sender = sender.clone();
            thread::spawn(move || {
                let result = snapshot::capture_now(&storage, &data_directory);
                busy.store(false, Ordering::Release);
                let message = result
                    .map(|summary| format!("手动快照完成：{}", summary.label()))
                    .unwrap_or_else(|error| format!("手动快照失败：{error}"));
                let _ = sender.send(ActionStatus {
                    message,
                    refresh: true,
                    report: None,
                });
            });
        });
    }
    install_snapshot_policy_callbacks(
        window,
        Arc::clone(&storage),
        Rc::clone(&ui_state),
        Rc::clone(&snapshot_state),
    );
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_toggle_exclusion(move || {
            let identity = ui_state.borrow().selected_identity.clone();
            let result = (|| -> Result<String> {
                let identity = identity.context("请先在主界面选择一个应用")?;
                let storage = storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
                let excluded = storage
                    .snapshot_exclusions()?
                    .iter()
                    .any(|candidate| candidate == &identity);
                storage.set_snapshot_excluded(&identity, !excluded, unix_time_ms())?;
                Ok(if excluded {
                    "已从快照排除名单恢复所选应用".to_owned()
                } else {
                    "已将所选应用加入独立快照排除名单".to_owned()
                })
            })();
            if let Some(window) = weak.upgrade() {
                window.set_action_status(
                    result
                        .as_ref()
                        .map_or_else(|error| format!("排除设置失败：{error}"), Clone::clone)
                        .into(),
                );
                if result.is_ok()
                    && let Err(error) =
                        refresh_snapshot_panel(&window, &storage, &ui_state, &snapshot_state)
                {
                    window.set_action_status(format!("快照设置刷新失败：{error}").into());
                }
            }
        });
    }
}

fn install_snapshot_policy_callbacks(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    ui_state: Rc<RefCell<UiState>>,
    snapshot_state: Rc<RefCell<SnapshotUiState>>,
) {
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_toggle_enabled(move || {
            update_and_refresh_snapshot_policy(
                &weak,
                &storage,
                &ui_state,
                &snapshot_state,
                |policy| policy.enabled = !policy.enabled,
            );
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_interval_selected(move |minutes| {
            update_and_refresh_snapshot_policy(
                &weak,
                &storage,
                &ui_state,
                &snapshot_state,
                |policy| policy.interval_minutes = minutes as u32,
            );
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_toggle_target(move || {
            update_and_refresh_snapshot_policy(
                &weak,
                &storage,
                &ui_state,
                &snapshot_state,
                |policy| policy.capture_all_displays = !policy.capture_all_displays,
            );
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_retention_selected(move |days, max_mib| {
            update_and_refresh_snapshot_policy(
                &weak,
                &storage,
                &ui_state,
                &snapshot_state,
                |policy| {
                    policy.retention_days = (days > 0).then_some(days as u32);
                    policy.max_bytes = max_mib.max(1) as u64 * 1024 * 1024;
                },
            );
        });
    }
}

fn update_and_refresh_snapshot_policy(
    weak: &slint::Weak<AppWindow>,
    storage: &Arc<Mutex<Storage>>,
    ui_state: &Rc<RefCell<UiState>>,
    snapshot_state: &Rc<RefCell<SnapshotUiState>>,
    update: impl FnOnce(&mut SnapshotPolicy),
) {
    let result = snapshot::run_heavy_task(|| -> Result<()> {
        let storage = storage
            .lock()
            .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
        let mut policy = storage.snapshot_policy()?;
        update(&mut policy);
        storage.set_snapshot_policy(policy, unix_time_ms())?;
        storage.apply_retention(unix_time_ms())?;
        Ok(())
    });
    if let Some(window) = weak.upgrade() {
        match result {
            Ok(()) => {
                window.set_action_status("快照策略已更新；固定周期从现在重新开始".into());
                if let Err(error) =
                    refresh_snapshot_panel(&window, storage, ui_state, snapshot_state)
                {
                    window.set_action_status(format!("快照设置刷新失败：{error}").into());
                }
            }
            Err(error) => window.set_action_status(format!("快照策略失败：{error}").into()),
        }
    }
}

fn refresh_snapshot_panel(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    ui_state: &Rc<RefCell<UiState>>,
    snapshot_state: &Rc<RefCell<SnapshotUiState>>,
) -> Result<()> {
    let (range_started, range_ended, selected_identity) = {
        let state = ui_state.borrow();
        (
            state.range_started_utc_ms,
            state.range_ended_utc_ms,
            state.selected_identity.clone(),
        )
    };
    let (policy, exclusions, slots) = {
        let storage = storage
            .lock()
            .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
        (
            storage.snapshot_policy()?,
            storage.snapshot_exclusions()?,
            storage.list_snapshot_slots(range_started, range_ended, 500)?,
        )
    };
    let rows = slots
        .iter()
        .map(|slot| SnapshotRow {
            time: snapshot_time_label(slot.slot_started_utc_ms).into(),
            display: slot
                .display
                .key
                .trim_start_matches(r"\\.\")
                .to_owned()
                .into(),
            result: if slot.success {
                format!("已保存 · {}", format_bytes(slot.plaintext_bytes)).into()
            } else {
                format!(
                    "缺失 · {}",
                    slot.missing_reason
                        .map(snapshot_reason_label)
                        .unwrap_or("未知原因")
                )
                .into()
            },
        })
        .collect::<Vec<_>>();
    let selected_index = snapshot_state
        .borrow()
        .selected_slot_id
        .and_then(|id| slots.iter().position(|slot| slot.id == id))
        .map_or(-1, |index| index as i32);
    {
        let mut state = snapshot_state.borrow_mut();
        state.slots = slots;
        if selected_index < 0 {
            state.selected_slot_id = None;
        }
    }
    window.set_snapshots(ModelRc::new(VecModel::from(rows)));
    window.set_snapshot_selected_index(selected_index);
    if selected_index < 0 {
        window.set_snapshot_preview(Image::default());
        window.set_snapshot_detail("请选择一条成功快照进行解密预览".into());
    }
    window.set_snapshot_policy_label(snapshot_policy_label(policy).into());
    let exclusion_label = match selected_identity {
        Some(identity) if exclusions.iter().any(|candidate| candidate == &identity) => {
            format!("{} · 当前已排除", window.get_selected_name())
        }
        Some(_) => format!("{} · 当前允许", window.get_selected_name()),
        None => "先在主界面选择应用".to_owned(),
    };
    window.set_snapshot_exclusion_label(exclusion_label.into());
    Ok(())
}

fn install_report_callback(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    ui_state: Rc<RefCell<UiState>>,
    busy: Arc<AtomicBool>,
    sender: mpsc::Sender<ActionStatus>,
) {
    let weak = window.as_weak();
    window.on_report_generate(move || {
        if busy.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(window) = weak.upgrade() {
            window.set_report_summary("正在根据当前时段的本地事实生成报告…".into());
            window.set_report_apps(ModelRc::new(VecModel::<ReportAppRow>::default()));
        }
        let (range_started, range_ended) = {
            let state = ui_state.borrow();
            (state.range_started_utc_ms, state.range_ended_utc_ms)
        };
        let storage = Arc::clone(&storage);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        thread::spawn(move || {
            let result = snapshot::run_heavy_task(|| {
                storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))
                    .and_then(|storage| {
                        storage
                            .generate_local_report(range_started, range_ended, unix_time_ms())
                            .map_err(Into::into)
                    })
            });
            busy.store(false, Ordering::Release);
            let (message, report) = match result {
                Ok(report) => (
                    format!("本地报告 #{} 已生成", report.id),
                    Some(Ok(report.id)),
                ),
                Err(error) => {
                    let message = error.to_string();
                    (format!("报告生成失败：{message}"), Some(Err(message)))
                }
            };
            let _ = sender.send(ActionStatus {
                message,
                refresh: false,
                report,
            });
        });
    });
}

fn refresh_report(window: &AppWindow, storage: &Arc<Mutex<Storage>>, report_id: i64) -> Result<()> {
    let report = storage
        .lock()
        .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?
        .load_local_report(report_id)?;
    render_report(window, &report);
    Ok(())
}

fn render_report(window: &AppWindow, report: &LocalReport) {
    let range_ms = report
        .range_ended_utc_ms
        .saturating_sub(report.range_started_utc_ms)
        .max(1) as u64;
    let coverage = report.covered_ms as f64 / range_ms as f64 * 100.0;
    let mouse = report
        .left_click_count
        .saturating_add(report.middle_click_count)
        .saturating_add(report.right_click_count);
    let reasons = if report.snapshot_reasons.is_empty() {
        "无".to_owned()
    } else {
        report
            .snapshot_reasons
            .iter()
            .map(|reason| format!("{} {}", reason.reason, reason.slot_count))
            .collect::<Vec<_>>()
            .join("、")
    };
    window.set_report_summary(
        format!(
            "报告 #{} · 规则 v{} · 时长 {} · 数据覆盖 {:.1}% · 应用 {} 个 · 明确缺口 {} 个\n键盘 {} · 鼠标 {} · 快照成功 {} · 快照缺失 {}（{}）",
            report.id,
            report.rules_version,
            format_duration(range_ms),
            coverage,
            report.applications.len(),
            report.gaps.len(),
            report.keyboard_count,
            mouse,
            report.snapshot_success_count,
            report.snapshot_missing_count,
            reasons,
        )
        .into(),
    );
    let rows = report
        .applications
        .iter()
        .map(|application| ReportAppRow {
            name: application.display_name.clone().into(),
            summary: format!(
                "打开 {} · 显示 {} · 聚焦 {} · 后台 {}",
                format_duration(application.opened_ms),
                format_duration(application.displayed_ms),
                format_duration(application.focused_ms),
                format_duration(application.background_ms),
            )
            .into(),
        })
        .collect::<Vec<_>>();
    window.set_report_apps(ModelRc::new(VecModel::from(rows)));
}

fn snapshot_policy_label(policy: SnapshotPolicy) -> String {
    let enabled = if policy.enabled {
        "已启用"
    } else {
        "已停用"
    };
    let target = if policy.capture_all_displays {
        "全部显示器"
    } else {
        "当前显示器"
    };
    let days = policy
        .retention_days
        .map_or_else(|| "永久".to_owned(), |days| format!("{days} 天"));
    format!(
        "{enabled} · 每 {} 分钟 · {target} · {days} / {}",
        policy.interval_minutes,
        format_bytes(policy.max_bytes)
    )
}

fn snapshot_reason_label(reason: SnapshotMissingReason) -> &'static str {
    match reason {
        SnapshotMissingReason::DesktopIdle => "桌面空闲",
        SnapshotMissingReason::GlobalPause => "全局暂停",
        SnapshotMissingReason::Locked => "锁屏",
        SnapshotMissingReason::Sleep => "睡眠/恢复",
        SnapshotMissingReason::SecureDesktop => "安全桌面/UAC",
        SnapshotMissingReason::SessionDisconnected => "会话断开",
        SnapshotMissingReason::RemoteSession => "远程会话",
        SnapshotMissingReason::PrivacyExclusion => "隐私排除",
        SnapshotMissingReason::CaptureFailed => "捕获失败",
        SnapshotMissingReason::LowDisk => "可用空间低于 2 GB",
        SnapshotMissingReason::RetentionCleaned => "保留策略已清理",
        SnapshotMissingReason::UserDeleted => "用户已删除",
        SnapshotMissingReason::BackupOmitted => "备份未包含图片",
        SnapshotMissingReason::RecoveryCorruption => "图片损坏，无法恢复",
    }
}

fn snapshot_time_label(timestamp_utc_ms: i64) -> String {
    let elapsed = unix_time_ms().saturating_sub(timestamp_utc_ms).max(0) as u64;
    if elapsed < 60_000 {
        format!("{} 秒前", elapsed / 1_000)
    } else if elapsed < 3_600_000 {
        format!("{} 分钟前", elapsed / 60_000)
    } else if elapsed < 86_400_000 {
        format!("{} 小时前", elapsed / 3_600_000)
    } else {
        format!("{} 天前", elapsed / 86_400_000)
    }
}

fn ratio(value_ms: u64, range_ms: f64) -> f32 {
    (value_ms as f64 / range_ms).clamp(0.0, 1.0) as f32
}

fn format_duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours} 小时 {minutes} 分")
    } else if minutes > 0 {
        format!("{minutes} 分 {seconds} 秒")
    } else {
        format!("{seconds} 秒")
    }
}

fn retention_label(policy: RetentionPolicy) -> String {
    let time = policy
        .days
        .map_or_else(|| "永久".to_owned(), |days| format!("{days} 天"));
    format!("{time} · {}", format_bytes(policy.max_bytes))
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[derive(Default)]
struct Options {
    recover: bool,
    shutdown: bool,
    uninstall_data: bool,
    background: bool,
    handshake_once: bool,
    snapshot_once: bool,
    pipe_name: Option<String>,
    data_directory: Option<PathBuf>,
}

impl Options {
    fn parse(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Self> {
        let mut options = Self::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.to_string_lossy().as_ref() {
                "--recover" => options.recover = true,
                "--shutdown" => options.shutdown = true,
                "--uninstall-data" => options.uninstall_data = true,
                "--background" => options.background = true,
                "--handshake-once" => options.handshake_once = true,
                "--snapshot-once" => options.snapshot_once = true,
                "--pipe-name" => {
                    options.pipe_name = Some(
                        arguments
                            .next()
                            .context("--pipe-name requires a value")?
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                "--data-dir" => {
                    options.data_directory = Some(PathBuf::from(
                        arguments.next().context("--data-dir requires a value")?,
                    ));
                }
                unknown => bail!("unknown argument: {unknown}"),
            }
        }
        Ok(options)
    }

    fn data_directory(&self) -> Result<PathBuf> {
        local_config::resolve(&self.control_directory()?)
    }

    fn control_directory(&self) -> Result<PathBuf> {
        if let Some(path) = &self.data_directory {
            return Ok(path.clone());
        }
        if let Some(path) = env::var_os("TIMELENS_DATA_DIR") {
            return Ok(PathBuf::from(path));
        }
        local_config::control_directory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_ui_state() -> UiState {
        UiState {
            horizon_started_utc_ms: 1_000,
            horizon_ended_utc_ms: 11_000,
            range_started_utc_ms: 1_000,
            range_ended_utc_ms: 11_000,
            selected_identity: None,
            application_identities: Vec::new(),
            follow_now: true,
        }
    }

    #[test]
    fn middle_drag_range_normalizes_reverse_direction_and_stops_following_now() {
        let mut state = fixed_ui_state();
        assert!(state.select_fraction_range(0.8, 0.2));
        assert_eq!(state.range_started_utc_ms, 3_000);
        assert_eq!(state.range_ended_utc_ms, 9_000);
        assert!(!state.follow_now);
    }

    #[test]
    fn middle_drag_range_rejects_clicks_and_clamps_outside_the_axis() {
        let mut state = fixed_ui_state();
        assert!(!state.select_fraction_range(0.5, 0.500_5));
        assert_eq!(
            (state.range_started_utc_ms, state.range_ended_utc_ms),
            (1_000, 11_000)
        );
        assert!(state.follow_now);
        assert!(state.select_fraction_range(-1.0, 2.0));
        assert_eq!(
            (state.range_started_utc_ms, state.range_ended_utc_ms),
            (1_000, 11_000)
        );
        assert!(!state.follow_now);
    }
}
