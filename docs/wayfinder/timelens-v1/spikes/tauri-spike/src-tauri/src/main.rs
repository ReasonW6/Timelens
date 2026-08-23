use std::{env, fs, path::PathBuf};

#[tauri::command]
fn load_timeline() -> Result<String, String> {
    let path = env::var("TIMELENS_SPIKE_TIMELINE").map_err(|error| error.to_string())?;
    fs::read_to_string(path).map_err(|error| error.to_string())
}

#[tauri::command]
fn record_pan(elapsed_ms: f64, max_step_gap_ms: f64, late_step_count: u32) -> Result<(), String> {
    let output =
        PathBuf::from(env::var("TIMELENS_SPIKE_OUTPUT").map_err(|error| error.to_string())?);
    let payload = serde_json::json!({
        "steps": 240,
        "elapsedMs": elapsed_ms,
        "maxStepGapMs": max_step_gap_ms,
        "lateStepCount": late_step_count
    });
    fs::write(output.join("ui-pan.json"), payload.to_string()).map_err(|error| error.to_string())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![load_timeline, record_pan])
        .run(tauri::generate_context!())
        .expect("failed to run Tauri spike");
}
