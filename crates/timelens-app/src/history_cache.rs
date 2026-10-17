use super::*;
use std::{
    collections::{BTreeSet, VecDeque},
    sync::mpsc,
    thread,
};

#[derive(Clone)]
struct Location {
    day: i32,
    key: SharedString,
    kind: i32,
    height: f32,
    top: f32,
}
struct Outline {
    rows: Vec<Location>,
    header: Option<HistoryItem>,
}
struct Loaded {
    epoch: u64,
    horizon: i32,
    days: Vec<(i32, Option<Day>)>,
}

#[derive(Default)]
pub(super) struct Paging {
    model: Option<Rc<VecModel<HistoryItem>>>,
    outlines: BTreeMap<i32, Outline>,
    layout: Vec<Location>,
    queue: VecDeque<i32>,
    queued: BTreeSet<i32>,
    pending: Option<mpsc::Receiver<anyhow::Result<Loaded>>>,
    inflight: Vec<i32>,
    epoch: u64,
    date: Option<NaiveDate>,
    today_requested: Option<Instant>,
    jump: Option<i32>,
    /// Today's activities when last shown, to tell when new ones arrive.
    today_count: Option<usize>,
}

/// Days back from today that a search covers.
const SEARCH_DAYS: i32 = 30;

fn active(window: &AppWindow) -> bool {
    window.window().is_visible() && !window.window().is_minimized()
}

fn queue(state: &mut HistoryState, offset: i32) {
    if offset >= 0 && offset <= state.horizon && state.paging.queued.insert(offset) {
        state.paging.queue.push_back(offset);
    }
}

fn needed(state: &HistoryState, scroll: f32, height: f32) -> BTreeSet<i32> {
    let mut offsets = BTreeSet::from([0]);
    for row in &state.paging.layout {
        if row.day >= 0 && row.top + row.height >= scroll && row.top <= scroll + height {
            offsets.extend((row.day - 2).max(0)..=(row.day + 2).min(state.horizon));
        }
    }
    if let Some(key) = &state.selected
        && let Some(date) = key
            .get(..10)
            .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
    {
        let offset = (Local::now().date_naive() - date).num_days();
        if (0..=i64::from(state.horizon)).contains(&offset) {
            offsets.insert(offset as i32);
        }
    }
    if let Some(offset) = state.paging.jump {
        offsets.extend((offset - 2).max(0)..=(offset + 2).min(state.horizon));
    }
    offsets
}

fn request(window: &AppWindow, storage: &Arc<Mutex<Storage>>, state: &mut HistoryState) {
    if state.paging.pending.is_some() || !active(window) {
        return;
    }
    let mut offsets = Vec::new();
    for _ in 0..DAYS_PER_LOAD {
        if let Some(offset) = state.paging.queue.pop_front() {
            offsets.push(offset);
        }
    }
    if offsets.is_empty() {
        return;
    }
    let previous = offsets
        .iter()
        .map(|offset| state.days.get(offset).cloned())
        .collect::<Vec<_>>();
    let epoch = state.paging.epoch;
    state.paging.inflight = offsets.clone();
    let (sender, receiver) = mpsc::channel();
    state.paging.pending = Some(receiver);
    let storage = Arc::clone(storage);
    let weak = window.as_weak();
    thread::spawn(move || {
        let result = (|| -> anyhow::Result<Loaded> {
            let storage = storage
                .lock()
                .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
            let horizon = reach(&storage).unwrap_or_default();
            let mut days = Vec::new();
            for (offset, previous) in offsets.into_iter().zip(previous) {
                days.push((offset, load_day(&storage, offset, previous.as_deref())?));
            }
            Ok(Loaded {
                epoch,
                horizon,
                days,
            })
        })();
        if sender.send(result).is_ok() {
            let _ = weak.upgrade_in_event_loop(|window| window.invoke_history_data_ready());
        }
    });
}

fn outline(day: &Day, query: &str) -> Outline {
    let (items, _) = flatten_days(std::iter::once(day), query, 0, 0, None);
    let header = items.iter().find(|item| item.kind == KIND_DAY).cloned();
    Outline {
        header,
        rows: items
            .into_iter()
            .skip(1)
            .map(|item| Location {
                day: day.offset,
                key: item.key,
                kind: item.kind,
                height: item.height,
                top: 0.0,
            })
            .collect(),
    }
}

fn placeholder(offset: i32) -> Outline {
    let date = Local::now().date_naive() - chrono::Duration::days(i64::from(offset));
    let day = Day {
        offset,
        date,
        entries: Vec::new(),
        unrecorded: Vec::new(),
        totals: Vec::new(),
        focus_ms: 0,
        source: None,
        system: Vec::new(),
    };
    let (title, subtitle) = day_title(&day);
    let header = HistoryItem {
        kind: KIND_DAY,
        key: format!("day:{date}").into(),
        height: DAY_HEIGHT,
        title: title.into(),
        subtitle: subtitle.into(),
        ..Default::default()
    };
    Outline {
        rows: vec![Location {
            day: offset,
            key: header.key.clone(),
            kind: KIND_DAY,
            height: DAY_HEIGHT,
            top: 0.0,
        }],
        header: Some(header),
    }
}

fn publish(window: &AppWindow, state: &mut HistoryState) {
    let scroll = -window.get_history_scroll();
    let height = window.get_history_visible_height().max(600.0);
    let old_anchor = (!window.get_history_follow())
        .then(|| {
            state
                .paging
                .layout
                .iter()
                .find(|row| row.top + row.height > scroll)
                .map(|row| (row.key.clone(), row.top))
        })
        .flatten();
    let render = (scroll - height, scroll + height * 2.0);
    let query = state.query.trim().to_lowercase();
    let (end, _) = flatten_days(
        std::iter::empty(),
        &query,
        state.loaded,
        state.horizon,
        None,
    );
    let mut shown = Vec::new();
    let mut positions = Vec::new();
    let mut layout = vec![Location {
        day: -1,
        key: "end".into(),
        kind: KIND_END,
        height: END_HEIGHT,
        top: 0.0,
    }];
    if render.0 <= END_HEIGHT {
        shown.push(end[0].clone());
        positions.push(0.0);
    }
    let mut anchors = Vec::new();
    let mut top = END_HEIGHT;
    for offset in (0..=state.loaded.max(0)).rev() {
        if let std::collections::btree_map::Entry::Vacant(entry) =
            state.paging.outlines.entry(offset)
        {
            if !query.is_empty() {
                continue;
            }
            entry.insert(placeholder(offset));
        }
        let day_top = top;
        let mut rich = BTreeMap::new();
        if let Some(day) = state.days.get(&offset) {
            let (items, _) = flatten_days(
                std::iter::once(day.as_ref()),
                &query,
                state.loaded,
                state.horizon,
                Some((render.0 - top + END_HEIGHT, render.1 - top + END_HEIGHT)),
            );
            rich.extend(
                items
                    .into_iter()
                    .skip(1)
                    .map(|item| (item.key.clone(), item)),
            );
            state.paging.outlines.insert(offset, outline(day, &query));
        }
        let outline = &state.paging.outlines[&offset];
        for location in &outline.rows {
            let mut location = location.clone();
            location.top = top;
            if top + location.height >= render.0 && top <= render.1 {
                let item = rich.remove(&location.key).unwrap_or_else(|| {
                    if location.kind == KIND_DAY {
                        outline.header.clone().unwrap_or_default()
                    } else {
                        HistoryItem {
                            key: location.key.clone(),
                            kind: location.kind,
                            height: location.height,
                            ..Default::default()
                        }
                    }
                });
                shown.push(item);
                positions.push(top);
            }
            top += location.height;
            layout.push(location);
        }
        if let Some(header) = &outline.header {
            anchors.push(DayAnchor {
                title: header.title.clone(),
                subtitle: header.subtitle.clone(),
                duration: header.duration.clone(),
                apps: header.time.clone(),
                shares: state
                    .days
                    .get(&offset)
                    .filter(|_| day_top <= render.1 && top >= render.0)
                    .map_or_else(ModelRc::default, |day| shares(&day.totals)),
                top: day_top,
                bottom: top,
            });
        }
    }
    window.set_history_content_height(top);
    if let Some((key, old_top)) = old_anchor
        && let Some(row) = layout.iter().find(|row| row.key == key)
        && row.top != old_top
    {
        window.set_history_scroll(-(scroll + row.top - old_top));
    }
    let target = state.paging.jump.and_then(|offset| {
        state
            .paging
            .outlines
            .get(&offset)
            .filter(|_| state.days.contains_key(&offset))
            .and_then(|outline| outline.header.as_ref())
            .map(|header| header.key.clone())
    });
    if let Some(key) = target {
        if let Some(row) = layout.iter().find(|row| row.key == key) {
            window.set_history_scroll(-row.top);
        }
        state.paging.jump = None;
    } else if state
        .paging
        .jump
        .is_some_and(|offset| state.days.contains_key(&offset))
    {
        state.paging.jump = None;
        window.set_action_status("这一天没有记录".into());
    }
    state.paging.layout = layout;
    window.set_history_positions(ModelRc::new(VecModel::from(positions)));
    if let Some(model) = &state.paging.model {
        patch(model, shown);
    } else {
        let model = Rc::new(VecModel::from(shown));
        window.set_history(ModelRc::from(Rc::clone(&model)));
        state.paging.model = Some(model);
    }
    window.set_day_anchors(ModelRc::new(VecModel::from(anchors)));
    window.set_history_selected(state.selected.clone().unwrap_or_default().into());
    describe(window, state, &query);
    let wanted = needed(state, -window.get_history_scroll(), height);
    state.days.retain(|offset, _| wanted.contains(offset));
    for offset in wanted {
        if !state.days.contains_key(&offset) {
            queue(state, offset);
        }
    }
    if -window.get_history_scroll() != scroll {
        publish(window, state);
    }
}

fn ymd(date: NaiveDate) -> i32 {
    date.year() * 10_000 + date.month() as i32 * 100 + date.day() as i32
}

/// What the toolbar and the search line say about the history on screen.
fn describe(window: &AppWindow, state: &mut HistoryState, query: &str) {
    let today = Local::now().date_naive();
    window.set_earliest_ymd(ymd(
        today - chrono::Duration::days(i64::from(state.horizon.max(0)))
    ));
    // The day at the top of the view.
    let scroll = -window.get_history_scroll();
    let offset = state
        .paging
        .layout
        .iter()
        .find(|row| row.day >= 0 && row.top + row.height > scroll)
        .map_or(0, |row| row.day);
    let date = today - chrono::Duration::days(i64::from(offset));
    window.set_history_day_ymd(ymd(date));
    window.set_history_day_label(
        match offset {
            0 => String::new(),
            1 => "昨天".to_owned(),
            _ => date.format("%-m月%-d日").to_string(),
        }
        .into(),
    );
    // New activity below, while the user reads further up.
    if let Some(count) = state.days.get(&0).map(|day| day.entries.len()) {
        if state.paging.today_count.is_some_and(|shown| count > shown)
            && !window.get_history_follow()
        {
            window.set_history_has_new(true);
        }
        state.paging.today_count = Some(count);
    }
    if query.is_empty() {
        window.set_history_search_status(SharedString::new());
        window.set_history_search_empty(false);
        return;
    }
    let until = SEARCH_DAYS.min(state.horizon.max(0));
    let searched = state.paging.outlines.range(0..=until).count() as i32;
    let found = state
        .paging
        .layout
        .iter()
        .filter(|row| row.kind == KIND_ENTRY)
        .count();
    let done =
        searched > until || (state.paging.queue.is_empty() && state.paging.pending.is_none());
    let shown = state.query.trim();
    window.set_history_search_status(
        if done {
            format!("“{shown}” · 最近一个月 · 找到 {found} 段")
        } else {
            format!(
                "正在搜索最近一个月… 已查 {searched}/{} 天，找到 {found} 段",
                until + 1
            )
        }
        .into(),
    );
    window.set_history_search_empty(done && found == 0);
}

/// The activity `delta` rows from the selected one, with its place in the
/// list; without a selection, the latest.
pub(super) fn step(state: &HistoryState, delta: i32) -> Option<(SharedString, f32, f32)> {
    let entries = state
        .paging
        .layout
        .iter()
        .filter(|row| row.kind == KIND_ENTRY)
        .collect::<Vec<_>>();
    let current = state
        .selected
        .as_deref()
        .and_then(|key| entries.iter().position(|row| row.key == key));
    let index = match current {
        Some(index) => index.checked_add_signed(delta as isize)?,
        None => entries.len().checked_sub(1)?,
    };
    entries
        .get(index)
        .map(|row| (row.key.clone(), row.top, row.height))
}

pub(super) fn refresh(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    state: &Rc<RefCell<HistoryState>>,
) -> anyhow::Result<()> {
    if !active(window) {
        return Ok(());
    }
    let mut state = state.borrow_mut();
    let date = Local::now().date_naive();
    let generation = GENERATION.load(Ordering::Acquire);
    if state.paging.date.is_some_and(|old| old != date) || state.generation != generation {
        let old_epoch = state.paging.epoch;
        state.days.clear();
        if let Some(model) = &state.paging.model {
            patch(model, Vec::new());
        }
        state.paging = Paging {
            epoch: old_epoch.wrapping_add(1),
            ..Default::default()
        };
        state.loaded = -1;
        state.generation = generation;
    }
    state.paging.date = Some(date);
    let received = state
        .paging
        .pending
        .as_ref()
        .map(|receiver| receiver.try_recv());
    if let Some(Ok(result)) = received {
        state.paging.pending = None;
        for offset in std::mem::take(&mut state.paging.inflight) {
            state.paging.queued.remove(&offset);
        }
        let loaded = result?;
        if loaded.epoch == state.paging.epoch {
            state.horizon = loaded.horizon;
            state.loaded = state.loaded.min(state.horizon).max(0);
            // A search typed before the first load returned started with too short a reach.
            extend_search(&mut state);
            for (offset, day) in loaded.days {
                state.paging.queued.remove(&offset);
                if let Some(day) = day {
                    let locations = outline(&day, &state.query);
                    state.paging.outlines.insert(offset, locations);
                    state.days.insert(offset, Arc::new(day));
                }
            }
        }
    } else if matches!(received, Some(Err(mpsc::TryRecvError::Disconnected))) {
        state.paging.pending = None;
        state.paging.queued.clear();
        state.paging.inflight.clear();
    }
    if state.loaded < 0 {
        state.loaded = 1;
        state.horizon = state.horizon.max(1);
        queue(&mut state, 0);
        queue(&mut state, 1);
    }
    if state
        .paging
        .today_requested
        .is_none_or(|at| at.elapsed() >= Duration::from_secs(2))
    {
        state.paging.today_requested = Some(Instant::now());
        queue(&mut state, 0);
    }
    if state.past_refreshed.elapsed() >= PAST_REFRESH {
        state.past_refreshed = Instant::now();
        queue(&mut state, 1);
    }
    let complete = state
        .paging
        .outlines
        .values()
        .filter(|outline| outline.rows.iter().any(|row| row.kind == KIND_ENTRY))
        .count();
    if complete < 2
        && state.loaded < state.horizon.min(14)
        && state.paging.pending.is_none()
        && state.paging.queue.is_empty()
    {
        let until = (state.loaded + DAYS_PER_LOAD).min(state.horizon);
        for offset in state.loaded + 1..=until {
            queue(&mut state, offset);
        }
        state.loaded = until;
    }
    publish(window, &mut state);
    request(window, storage, &mut state);
    Ok(())
}

pub(super) fn more(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    state: &Rc<RefCell<HistoryState>>,
) {
    if !active(window) || state.try_borrow().is_err() {
        return;
    }
    let mut state = state.borrow_mut();
    let until = (state.loaded + DAYS_PER_LOAD).min(state.horizon);
    for offset in state.loaded + 1..=until {
        queue(&mut state, offset);
    }
    state.loaded = until;
    publish(window, &mut state);
    request(window, storage, &mut state);
}

pub(super) fn search(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    state: &Rc<RefCell<HistoryState>>,
    query: &str,
) {
    let mut state = state.borrow_mut();
    restart_search(&mut state, query);
    publish(window, &mut state);
    request(window, storage, &mut state);
}

/// The request in flight is kept: its worker cannot be stopped, so replacing it
/// on every keystroke would only queue more database work. The new epoch
/// discards its result, and the next request starts once it ends.
fn restart_search(state: &mut HistoryState, query: &str) {
    state.query = query.to_owned();
    state.paging.epoch = state.paging.epoch.wrapping_add(1);
    state.paging.inflight.clear();
    state.paging.queue.clear();
    state.paging.queued.clear();
    state.paging.outlines.clear();
    // Days already read are searched at once; only the rest are read again.
    let resident = state
        .days
        .iter()
        .map(|(offset, day)| (*offset, outline(day, query)))
        .collect::<Vec<_>>();
    state.paging.outlines.extend(resident);
    for offset in 0..=state.loaded {
        if !state.days.contains_key(&offset) {
            queue(state, offset);
        }
    }
    extend_search(state);
}

/// A search covers the recent month, once the oldest recorded day is known.
fn extend_search(state: &mut HistoryState) {
    let until = SEARCH_DAYS.min(state.horizon);
    if !state.query.trim().is_empty() && state.loaded < until {
        for offset in state.loaded + 1..=until {
            if !state.days.contains_key(&offset) {
                queue(state, offset);
            }
        }
        state.loaded = until;
    }
}

pub(super) fn jump(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    state: &Rc<RefCell<HistoryState>>,
    offset: i32,
) {
    let mut state = state.borrow_mut();
    clear_query(&mut state);
    state.paging.epoch = state.paging.epoch.wrapping_add(1);
    state.paging.pending = None;
    state.paging.inflight.clear();
    state.paging.queue.clear();
    state.paging.queued.clear();
    state.paging.jump = Some(offset);
    state.loaded = state.loaded.max(offset);
    queue(&mut state, offset);
    for day in (offset - 2).max(0)..=(offset + 2).min(state.horizon) {
        queue(&mut state, day);
    }
    window.set_history_query(SharedString::new());
    window.set_history_follow(false);
    publish(window, &mut state);
    request(window, storage, &mut state);
}

fn clear_query(state: &mut HistoryState) {
    if !state.query.is_empty() {
        state.query.clear();
        state.paging.outlines.clear();
    }
}

pub(super) fn viewport(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    state: &Rc<RefCell<HistoryState>>,
) {
    if !active(window) || state.try_borrow().is_err() {
        return;
    }
    let mut state = state.borrow_mut();
    publish(window, &mut state);
    request(window, storage, &mut state);
}

pub(super) fn reset(state: &mut HistoryState) {
    if let Some(model) = &state.paging.model {
        patch(model, Vec::new());
    }
    state.paging = Paging {
        epoch: state.paging.epoch.wrapping_add(1),
        ..Default::default()
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    fn day(offset: i32) -> Arc<Day> {
        let snapshot = super::super::tests::history_fixture(1_700_001_500_000);
        Arc::new(build_day(
            offset,
            Local::now().date_naive() - chrono::Duration::days(i64::from(offset)),
            &snapshot,
            None,
        ))
    }

    #[test]
    fn distant_days_retain_only_locations_and_selected_days_stay_resident() {
        let mut state = HistoryState::new();
        state.horizon = 60;
        state.loaded = 60;
        for offset in 0..=60 {
            let data = day(offset);
            state.paging.outlines.insert(offset, outline(&data, ""));
            state.days.insert(offset, data);
        }
        let selected_date = Local::now().date_naive() - chrono::Duration::days(10);
        state.selected = Some(format!("{selected_date}:123:fixture"));
        state.paging.layout = vec![Location {
            day: 30,
            key: "visible".into(),
            kind: KIND_ENTRY,
            height: 100.0,
            top: 100.0,
        }];
        let wanted = needed(&state, 100.0, 50.0);
        state.days.retain(|offset, _| wanted.contains(offset));
        assert_eq!(
            state.days.keys().copied().collect::<Vec<_>>(),
            [0, 10, 28, 29, 30, 31, 32]
        );
        assert_eq!(state.paging.outlines.len(), 61);
        for summary in state.paging.outlines.values() {
            assert!(
                summary
                    .header
                    .as_ref()
                    .is_none_or(|header| header.icon.size().width == 0
                        && header.stack.row_count() == 0
                        && header.shares.row_count() == 0)
            );
        }
    }

    #[test]
    fn location_heights_and_keys_match_the_existing_full_history() {
        let mut state = HistoryState::new();
        state.loaded = 3;
        state.horizon = 10;
        for offset in 0..=3 {
            state.days.insert(offset, day(offset));
        }
        let (full, _) = flatten(&state);
        let mut locations = vec![("end".to_owned(), END_HEIGHT)];
        for data in state.days.values().rev() {
            locations.extend(
                outline(data, "")
                    .rows
                    .into_iter()
                    .map(|row| (row.key.to_string(), row.height)),
            );
        }
        assert_eq!(
            locations,
            full.iter()
                .map(|row| (row.key.to_string(), row.height))
                .collect::<Vec<_>>()
        );
        let before = row_tops(full.clone().into_iter());
        let mut top = 0.0;
        let after = locations
            .iter()
            .map(|(key, height)| {
                let current = (SharedString::from(key.as_str()), top, *height);
                top += height;
                current
            })
            .collect::<Vec<_>>();
        assert_eq!(before, after);
    }

    #[test]
    fn reset_releases_view_images_and_cancels_the_previous_epoch() {
        let mut state = HistoryState::new();
        let model = Rc::new(VecModel::from(vec![HistoryItem {
            key: "previous".into(),
            ..Default::default()
        }]));
        state.paging.model = Some(Rc::clone(&model));
        state.paging.epoch = 10;
        let (sender, receiver) = mpsc::channel();
        state.paging.pending = Some(receiver);
        reset(&mut state);
        assert_eq!(model.row_count(), 0);
        assert_eq!(state.paging.epoch, 11);
        assert!(
            sender
                .send(Ok(Loaded {
                    epoch: 10,
                    horizon: 10,
                    days: vec![]
                }))
                .is_err()
        );
    }

    #[test]
    fn a_new_search_waits_for_the_running_request_instead_of_adding_workers() {
        let mut state = HistoryState::new();
        state.horizon = 60;
        state.loaded = 3;
        state.paging.epoch = 4;
        let (_sender, receiver) = mpsc::channel();
        state.paging.pending = Some(receiver);
        state.paging.inflight = vec![0, 1, 2];
        restart_search(&mut state, "a");
        restart_search(&mut state, "ab");
        assert!(state.paging.pending.is_some());
        assert!(state.paging.inflight.is_empty());
        assert_eq!(state.paging.epoch, 6);
        assert_eq!(state.loaded, SEARCH_DAYS);
        assert_eq!(state.paging.queue.len(), SEARCH_DAYS as usize + 1);
    }

    #[test]
    fn a_search_typed_before_the_horizon_was_known_reaches_the_recent_month() {
        let mut state = HistoryState::new();
        state.horizon = 1;
        state.loaded = 1;
        restart_search(&mut state, "editor");
        assert_eq!(state.loaded, 1);
        state.horizon = 60;
        extend_search(&mut state);
        assert_eq!(state.loaded, SEARCH_DAYS);
        assert!(state.paging.queued.contains(&SEARCH_DAYS));
        // Without a query the reach is left to scrolling.
        let mut idle = HistoryState::new();
        idle.horizon = 60;
        idle.loaded = 1;
        extend_search(&mut idle);
        assert_eq!(idle.loaded, 1);
    }

    #[test]
    fn leaving_search_discards_filtered_locations_without_losing_selection() {
        let mut state = HistoryState::new();
        state.query = "unmatched".into();
        state.loaded = 30;
        state.selected = Some("selected".into());
        let data = day(10);
        state
            .paging
            .outlines
            .insert(10, outline(&data, &state.query));
        state.days.insert(10, data);
        clear_query(&mut state);
        assert!(state.query.is_empty());
        assert!(state.paging.outlines.is_empty());
        assert_eq!(state.loaded, 30);
        assert_eq!(state.selected.as_deref(), Some("selected"));
        assert!(state.days.contains_key(&10));
    }
}
