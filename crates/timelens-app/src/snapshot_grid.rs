//! The snapshot grid: thumbnails for the snapshots in view, and ticking
//! snapshots to delete several at once.
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use anyhow::anyhow;
use slint::{ComponentHandle, Image, Model};
use timelens_storage::Storage;

use crate::{AppWindow, SnapshotUiState, snapshot};

/// Bumped whenever the view moves, so decoding for snapshots no longer in view stops.
static THUMBNAIL_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Until the grid reports what it shows, assume its first screenful.
const FIRST_SCREEN: (usize, usize) = (0, 23);

/// Decodes thumbnails for the visible snapshots off the UI thread and drops
/// those more than a screen away, so memory follows the view, not the list.
pub(crate) fn load_thumbnails(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    state: &SnapshotUiState,
) {
    let (first, last) = state.visible.unwrap_or(FIRST_SCREEN);
    let screen = last.saturating_sub(first) + 1;
    let kept = first.saturating_sub(screen)..=last.saturating_add(screen);
    let rows = window.get_snapshots();
    let mut wanted = Vec::new();
    for (index, slot) in state.slots.iter().enumerate() {
        let Some(mut row) = rows.row_data(index) else {
            break;
        };
        let shown = row.thumb.size().width > 0;
        if shown && !kept.contains(&index) {
            row.thumb = Image::default();
            rows.set_row_data(index, row);
        } else if !shown && slot.success && (first..=last).contains(&index) {
            wanted.push(slot.id);
        }
    }
    let generation = THUMBNAIL_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    if wanted.is_empty() {
        return;
    }
    let storage = Arc::clone(storage);
    let window = window.as_weak();
    thread::spawn(move || {
        let current = || THUMBNAIL_GENERATION.load(Ordering::Acquire) == generation;
        for slot_id in wanted {
            let pixels = snapshot::run_heavy_task(|| {
                if !current() {
                    return Ok(None);
                }
                let image = storage
                    .lock()
                    .map_err(|_| anyhow!("storage lock poisoned"))?
                    .load_snapshot_image(slot_id)?;
                snapshot::decode_thumbnail(&image.webp).map(Some)
            });
            match pixels {
                Ok(Some(pixels)) => {
                    let identity = slot_id.to_string();
                    let _ = window.upgrade_in_event_loop(move |window| {
                        let rows = window.get_snapshots();
                        if let Some((index, mut row)) = rows
                            .iter()
                            .enumerate()
                            .find(|(_, row)| row.identity == identity)
                        {
                            row.thumb = Image::from_rgb8(pixels);
                            rows.set_row_data(index, row);
                        }
                    });
                }
                // A newer view took over.
                Ok(None) => return,
                // The tile keeps its placeholder; opening it reports the error.
                Err(_) => {}
            }
        }
    });
}

/// Ticks or unticks one snapshot. With `span`, ticks every image between the
/// snapshot ticked last and this one instead.
pub(crate) fn tick(state: &mut SnapshotUiState, index: usize, span: bool) {
    let Some(slot) = state.slots.get(index).filter(|slot| slot.success) else {
        return;
    };
    let id = slot.id;
    let anchor = state
        .anchor
        .filter(|_| span)
        .and_then(|anchor| state.slots.iter().position(|slot| slot.id == anchor));
    match anchor {
        Some(anchor) => state.checked.extend(
            state.slots[anchor.min(index)..=anchor.max(index)]
                .iter()
                .filter(|slot| slot.success)
                .map(|slot| slot.id),
        ),
        None => {
            if !state.checked.remove(&id) {
                state.checked.insert(id);
            }
        }
    }
    state.anchor = Some(id);
}

pub(crate) fn tick_all(state: &mut SnapshotUiState, all: bool) {
    state.checked = if all {
        state
            .slots
            .iter()
            .filter(|slot| slot.success)
            .map(|slot| slot.id)
            .collect()
    } else {
        BTreeSet::new()
    };
    state.anchor = None;
}

/// Shows the ticks on the grid.
pub(crate) fn show_ticks(window: &AppWindow, state: &SnapshotUiState) {
    let rows = window.get_snapshots();
    for (index, slot) in state.slots.iter().enumerate() {
        let Some(mut row) = rows.row_data(index) else {
            break;
        };
        let checked = state.checked.contains(&slot.id);
        if row.checked != checked {
            row.checked = checked;
            rows.set_row_data(index, row);
        }
    }
    window.set_snapshot_checked_count(state.checked.len() as i32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use timelens_storage::{SnapshotDisplay, SnapshotSlot, SnapshotTrigger};

    fn state(success: &[bool]) -> SnapshotUiState {
        SnapshotUiState {
            slots: success
                .iter()
                .enumerate()
                .map(|(index, &success)| SnapshotSlot {
                    id: index as i64 + 1,
                    slot_started_utc_ms: index as i64 * 60_000,
                    captured_at_utc_ms: None,
                    display: SnapshotDisplay::session(),
                    pixel_width: 0,
                    pixel_height: 0,
                    trigger: SnapshotTrigger::Scheduled,
                    success,
                    missing_reason: None,
                    plaintext_bytes: 0,
                })
                .collect(),
            ..Default::default()
        }
    }

    fn ticked(state: &SnapshotUiState) -> Vec<i64> {
        state.checked.iter().copied().collect()
    }

    #[test]
    fn a_click_toggles_and_shift_ticks_the_span_in_either_direction() {
        let mut state = state(&[true, true, false, true, true, true]);
        tick(&mut state, 1, false);
        tick(&mut state, 4, true);
        // The missing slot in between has no image to delete.
        assert_eq!(ticked(&state), [2, 4, 5]);
        tick(&mut state, 4, false);
        assert_eq!(ticked(&state), [2, 4]);
        tick(&mut state, 0, true);
        assert_eq!(ticked(&state), [1, 2, 4, 5]);
    }

    #[test]
    fn missing_slots_are_never_ticked_and_shift_without_a_start_toggles() {
        let mut state = state(&[false, true, true]);
        tick(&mut state, 0, false);
        assert!(state.checked.is_empty());
        tick(&mut state, 2, true);
        assert_eq!(ticked(&state), [3]);
        tick_all(&mut state, true);
        assert_eq!(ticked(&state), [2, 3]);
        tick_all(&mut state, false);
        assert!(state.checked.is_empty());
        assert_eq!(state.anchor, None);
    }
}
