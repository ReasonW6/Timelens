use serde::Deserialize;
use slint::{ModelRc, Timer, TimerMode, VecModel};
use std::{
    cell::Cell,
    env, fs,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

slint::slint! {
    export struct Segment {
        start-minute: float,
        duration-minutes: float,
        lane: int,
        state: int,
    }

    export component SpikeWindow inherits Window {
        title: "Timelens Slint Spike";
        width: 1100px;
        height: 680px;
        background: #11151c;

        in property <[Segment]> segments;
        in-out property <float> pan-minute: 0;
        in-out property <float> zoom: 0.16;

        VerticalLayout {
            padding: 18px;
            spacing: 12px;

            Text {
                text: "Slint · 10,000 segment timeline";
                color: #f5f7fa;
                font-size: 24px;
                font-weight: 600;
            }
            Text {
                text: "Pan " + round(root.pan-minute) + " min  ·  Zoom " + round(root.zoom * 100) + "%";
                color: #98a2b3;
                font-size: 14px;
            }
            Rectangle {
                clip: true;
                background: #181e27;
                border-radius: 8px;

                for segment in root.segments: Rectangle {
                    x: ((segment.start-minute - root.pan-minute) * root.zoom) * 1px;
                    y: (segment.lane * 42 + 12) * 1px;
                    width: Math.max(1, segment.duration-minutes * root.zoom) * 1px;
                    height: 28px;
                    border-radius: 3px;
                    background: segment.state == 0 ? #68a7ff :
                                segment.state == 1 ? #6ce5a1 : #8a7cf6;
                }
            }
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TimelineSegment {
    start_minute: u32,
    duration_minutes: u32,
    lane: i32,
    state: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let timeline_path = PathBuf::from(env::var("TIMELENS_SPIKE_TIMELINE")?);
    let output_dir = PathBuf::from(env::var("TIMELENS_SPIKE_OUTPUT")?);
    let source: Vec<TimelineSegment> = serde_json::from_slice(&fs::read(timeline_path)?)?;
    let source = Rc::new(source);

    let window = SpikeWindow::new()?;
    window.set_zoom(1.0);
    window.set_segments(visible_segments(&source, 0.0, 1.0));

    let step = Rc::new(Cell::new(0_u32));
    let pan_started = Rc::new(Cell::new(None::<Instant>));
    let last_tick = Rc::new(Cell::new(None::<Instant>));
    let max_step_gap_ms = Rc::new(Cell::new(0_f64));
    let late_step_count = Rc::new(Cell::new(0_u32));
    let weak = window.as_weak();
    let timer = Rc::new(Timer::default());
    timer.start(TimerMode::Repeated, Duration::from_millis(16), {
        let step = step.clone();
        let pan_started = pan_started.clone();
        let last_tick = last_tick.clone();
        let max_step_gap_ms = max_step_gap_ms.clone();
        let late_step_count = late_step_count.clone();
        let source = source.clone();
        let timer = timer.clone();
        move || {
            let Some(window) = weak.upgrade() else { return };
            let now = Instant::now();
            if pan_started.get().is_none() {
                pan_started.set(Some(now));
            }
            if let Some(previous) = last_tick.replace(Some(now)) {
                let gap_ms = now.duration_since(previous).as_secs_f64() * 1000.0;
                max_step_gap_ms.set(max_step_gap_ms.get().max(gap_ms));
                if gap_ms > 25.0 {
                    late_step_count.set(late_step_count.get() + 1);
                }
            }
            let next = step.get() + 1;
            step.set(next);
            let pan = (next as f32 * 53.0) % 42_000.0;
            let zoom = 0.8 + (next % 40) as f32 * 0.015;
            window.set_pan_minute(pan);
            window.set_zoom(zoom);
            window.set_segments(visible_segments(&source, pan, zoom));
            if next == 240 {
                timer.stop();
                if let Some(started) = pan_started.get() {
                    let payload = format!(
                        "{{\"steps\":240,\"elapsedMs\":{:.3},\"maxStepGapMs\":{:.3},\"lateStepCount\":{}}}",
                        started.elapsed().as_secs_f64() * 1000.0,
                        max_step_gap_ms.get(),
                        late_step_count.get()
                    );
                    let _ = fs::write(output_dir.join("ui-pan.json"), payload);
                }
            }
        }
    });
    window.run()?;
    Ok(())
}

fn visible_segments(source: &[TimelineSegment], pan: f32, zoom: f32) -> ModelRc<Segment> {
    let right = pan + 1_100.0 / zoom;
    let rows = source
        .iter()
        .filter(|segment| {
            let start = segment.start_minute as f32;
            let end = start + segment.duration_minutes as f32;
            end >= pan && start <= right
        })
        .map(|segment| Segment {
            start_minute: segment.start_minute as f32,
            duration_minutes: segment.duration_minutes as f32,
            lane: segment.lane,
            state: match segment.state.as_str() {
                "displayed" => 0,
                "focused" => 1,
                _ => 2,
            },
        })
        .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}
