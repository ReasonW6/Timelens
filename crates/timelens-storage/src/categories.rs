//! Application categories: user labels chosen by hand or by folder, and the
//! focused spans the statistics trends are counted from.
use super::collection::merge_group;
use super::*;
use std::collections::{BTreeMap, BTreeSet};

/// A user label for applications, such as Work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Category {
    pub id: i64,
    /// The key of a category every dataset starts with, such as `work`, whose
    /// name follows the interface language until renamed.
    pub preset: Option<String>,
    /// The user's name for it; a preset without one keeps its own.
    pub name: Option<String>,
    /// Which of the interface's category colors it wears.
    pub color: u32,
    /// Folders whose applications fall into it, lowercase with backslashes.
    pub folders: Vec<String>,
}

/// How an application's category is chosen.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CategoryChoice {
    /// By folder rule, or none when no folder holds it.
    Auto,
    /// By hand: a category, or explicitly none.
    Manual(Option<i64>),
}

/// The category an application falls into, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationCategory {
    pub category: Option<i64>,
    pub choice: CategoryChoice,
    /// The folder rule that chose it, if one did.
    pub folder: Option<String>,
}

/// When each application, by its merge group's root, had the focus: each
/// list in order and not overlapping.
pub type FocusSpans = BTreeMap<String, Vec<(i64, i64)>>;

/// How many category colors the interface has.
pub const CATEGORY_COLORS: u32 = 10;
/// The longest category name, in characters.
pub const CATEGORY_NAME_LIMIT: usize = 40;

/// A folder as rules keep it: backslashes, lowercase, no trailing separator.
pub fn normalize_folder(folder: &str) -> Option<String> {
    let folder = folder.trim().replace('/', "\\").to_lowercase();
    let folder = folder.trim_end_matches('\\');
    (!folder.is_empty()).then(|| folder.to_owned())
}

/// Whether `path`, normalized like a folder, lies under `folder`.
fn under(path: &str, folder: &str) -> bool {
    path.strip_prefix(folder)
        .is_some_and(|rest| rest.starts_with('\\'))
}

/// What deciding categories needs, read once.
struct Rules {
    edges: Vec<(String, String)>,
    /// Manual choices by identity, with when they were made.
    manual: BTreeMap<String, (Option<i64>, i64)>,
    /// Every folder rule, longest first.
    folders: Vec<(String, i64)>,
    /// Recorded executables by identity.
    paths: BTreeMap<String, BTreeSet<String>>,
}

impl Rules {
    /// The choice by hand a merge group shares: the one made last.
    fn manual_choice(&self, members: &BTreeSet<String>) -> Option<Option<i64>> {
        members
            .iter()
            .filter_map(|member| self.manual.get(member))
            .max_by_key(|(_, at)| *at)
            .map(|(category, _)| *category)
    }

    fn decide(&self, identity: &str) -> ApplicationCategory {
        let members = merge_group(identity, &self.edges);
        if let Some(category) = self.manual_choice(&members) {
            return ApplicationCategory {
                category,
                choice: CategoryChoice::Manual(category),
                folder: None,
            };
        }
        let mut paths = BTreeSet::new();
        for member in &members {
            if let Some(path) = member.strip_prefix("path:") {
                paths.insert(path.to_owned());
            }
            if let Some(recorded) = self.paths.get(member) {
                paths.extend(recorded.iter().cloned());
            }
        }
        let rule = self
            .folders
            .iter()
            .find(|(folder, _)| paths.iter().any(|path| under(path, folder)));
        ApplicationCategory {
            category: rule.map(|(_, category)| *category),
            choice: CategoryChoice::Auto,
            folder: rule.map(|(folder, _)| folder.clone()),
        }
    }
}

impl Storage {
    /// Every category in order, with its folders.
    pub fn categories(&self) -> Result<Vec<Category>> {
        let mut folders = BTreeMap::<i64, Vec<String>>::new();
        let mut statement = self.connection.prepare(
            "SELECT category_id, folder FROM application_category_folders ORDER BY folder",
        )?;
        for row in statement.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get(1)?)))? {
            let (id, folder) = row?;
            folders.entry(id).or_default().push(folder);
        }
        let mut statement = self.connection.prepare(
            "SELECT category_id, preset, name, color FROM application_categories
             ORDER BY position, category_id",
        )?;
        let rows = statement.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (id, preset, name, color) = row?;
            Ok(Category {
                id,
                preset,
                name,
                color: u32::try_from(color).unwrap_or_default(),
                folders: folders.remove(&id).unwrap_or_default(),
            })
        })
        .collect()
    }

    /// Add a category after the others, in the first color none wears yet.
    pub fn add_category(&self, name: &str) -> Result<i64> {
        let name = category_name(name)?;
        let used = self
            .categories()?
            .into_iter()
            .map(|c| c.color)
            .collect::<BTreeSet<_>>();
        let color = (0..CATEGORY_COLORS)
            .find(|color| !used.contains(color))
            .unwrap_or(used.len() as u32 % CATEGORY_COLORS);
        self.connection.execute(
            "INSERT INTO application_categories(name, color, position)
             SELECT ?1, ?2, COALESCE(MAX(position), -1) + 1 FROM application_categories",
            params![name, color],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Rename a category; a preset given no name takes its own back.
    pub fn rename_category(&self, id: i64, name: &str) -> Result<()> {
        let name = match category_name(name) {
            Ok(name) => Some(name),
            Err(_) if name.trim().is_empty() => None,
            Err(error) => return Err(error),
        };
        let changed = self.connection.execute(
            "UPDATE application_categories SET name = ?2
             WHERE category_id = ?1 AND (?2 IS NOT NULL OR preset IS NOT NULL)",
            params![id, name],
        )?;
        if changed == 0 {
            return Err(StorageError::Integrity("这个分类不能使用这个名称".into()));
        }
        Ok(())
    }

    /// Delete a category. Its folder rules go with it, and applications put in
    /// it by hand are decided by folder again, with every application merged
    /// with them, so no older choice of a member takes its place.
    pub fn delete_category(&self, id: i64) -> Result<()> {
        let rules = self.category_rules()?;
        let mut cleared = BTreeSet::new();
        for (identity, (category, _)) in &rules.manual {
            if *category != Some(id) || cleared.contains(identity) {
                continue;
            }
            let members = merge_group(identity, &rules.edges);
            // Only a group showing this category changes; one whose newer
            // choice is another keeps it.
            if rules.manual_choice(&members) == Some(Some(id)) {
                cleared.extend(members);
            }
        }
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        for member in &cleared {
            tx.execute(
                "DELETE FROM application_category_assignments WHERE application_identity = ?1",
                [member],
            )?;
        }
        tx.execute(
            "DELETE FROM application_categories WHERE category_id = ?1",
            [id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Let applications under `folder` fall into a category. A folder belongs
    /// to one category only.
    pub fn add_category_folder(&self, id: i64, folder: &str) -> Result<()> {
        let folder = normalize_folder(folder)
            .ok_or_else(|| StorageError::Integrity("目录不能为空".into()))?;
        self.connection.execute(
            "INSERT INTO application_category_folders(folder, category_id) VALUES (?1, ?2)",
            params![folder, id],
        )?;
        Ok(())
    }

    pub fn remove_category_folder(&self, folder: &str) -> Result<()> {
        if let Some(folder) = normalize_folder(folder) {
            self.connection.execute(
                "DELETE FROM application_category_folders WHERE folder = ?1",
                [folder],
            )?;
        }
        Ok(())
    }

    /// Choose the category of an application and those merged with it.
    pub fn set_application_category(&self, identity: &str, choice: CategoryChoice) -> Result<()> {
        let members = self.merged_members(identity)?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        for member in &members {
            match choice {
                CategoryChoice::Auto => tx.execute(
                    "DELETE FROM application_category_assignments WHERE application_identity = ?1",
                    [member],
                )?,
                CategoryChoice::Manual(category) => tx.execute(
                    "INSERT OR REPLACE INTO application_category_assignments
                     (application_identity, category_id, assigned_utc_ms) VALUES (?1, ?2, ?3)",
                    params![member, category, unix_time_ms()],
                )?,
            };
        }
        tx.commit()?;
        Ok(())
    }

    /// The category an application and those merged with it fall into.
    pub fn application_category(&self, identity: &str) -> Result<ApplicationCategory> {
        let edges = self.merge_edges()?;
        let members = merge_group(identity, &edges);
        Ok(self
            .category_rules_for(edges, Some(&members))?
            .decide(identity))
    }

    /// The category of each application given, by its merge group's root as
    /// the statistics name it.
    pub fn application_categories<'a>(
        &self,
        identities: impl IntoIterator<Item = &'a str>,
    ) -> Result<BTreeMap<String, Option<i64>>> {
        let rules = self.category_rules()?;
        Ok(identities
            .into_iter()
            .map(|identity| (identity.to_owned(), rules.decide(identity).category))
            .collect())
    }

    fn category_rules(&self) -> Result<Rules> {
        self.category_rules_for(self.merge_edges()?, None)
    }

    /// The rules, with the choices by hand and the recorded executables of
    /// `only` alone when given, which is all one merge group needs.
    fn category_rules_for(
        &self,
        edges: Vec<(String, String)>,
        only: Option<&BTreeSet<String>>,
    ) -> Result<Rules> {
        let mut manual = BTreeMap::new();
        let mut paths = BTreeMap::<String, BTreeSet<String>>::new();
        let mut add_path = |identity: String, path: String| {
            if let Some(path) = normalize_folder(&path) {
                paths.entry(identity).or_default().insert(path);
            }
        };
        match only {
            None => {
                manual = self
                    .connection
                    .prepare(
                        "SELECT application_identity, category_id, assigned_utc_ms
                         FROM application_category_assignments",
                    )?
                    .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
                    .collect::<rusqlite::Result<_>>()?;
                let mut statement = self.connection.prepare(
                    "SELECT DISTINCT application_identity, executable_path
                     FROM application_metadata_revisions WHERE executable_path IS NOT NULL",
                )?;
                for row in statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))? {
                    let (identity, path) = row?;
                    add_path(identity, path);
                }
            }
            Some(members) => {
                let mut choice = self.connection.prepare(
                    "SELECT category_id, assigned_utc_ms FROM application_category_assignments
                     WHERE application_identity = ?1",
                )?;
                let mut recorded = self.connection.prepare(
                    "SELECT DISTINCT executable_path FROM application_metadata_revisions
                     WHERE application_identity = ?1 AND executable_path IS NOT NULL",
                )?;
                for member in members {
                    if let Some(made) = choice
                        .query_row([member], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?
                    {
                        manual.insert(member.clone(), made);
                    }
                    for path in recorded.query_map([member], |r| r.get(0))? {
                        add_path(member.clone(), path?);
                    }
                }
            }
        }
        let mut folders = self
            .connection
            .prepare("SELECT folder, category_id FROM application_category_folders")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        folders.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Ok(Rules {
            edges,
            manual,
            folders,
            paths,
        })
    }

    /// When each application had the focus within `start..end`, clipped to
    /// it, by its merge group's root. Like the application totals, an
    /// application's focus counts once however many of its windows had it,
    /// and Windows' own surfaces are left out. Nothing is told past now.
    pub fn focused_spans(&self, start: i64, end: i64) -> Result<FocusSpans> {
        let end = end.min(unix_time_ms());
        let mut spans = FocusSpans::new();
        if end <= start {
            return Ok(spans);
        }
        let hidden = self.system_component_identities()?;
        let edges = self.merge_edges()?;
        let mut roots = BTreeMap::<String, String>::new();
        let mut statement = self.connection.prepare(
            "SELECT instances.application_identity, states.started_utc_ms,
                    COALESCE(states.ended_utc_ms, ?2)
             FROM window_state_intervals AS states
             JOIN window_instances AS instances
               ON instances.instance_id = states.window_instance_id
             WHERE states.focused
               AND states.started_utc_ms < ?2
               AND COALESCE(states.ended_utc_ms, ?2) > ?1",
        )?;
        let rows = statement.query_map(params![start, end], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (identity, started, ended) = row?;
            if hidden.contains(&identity) {
                continue;
            }
            let (started, ended) = clipped_interval(started, ended, start, end);
            if ended <= started {
                continue;
            }
            let root = roots
                .entry(identity)
                .or_insert_with_key(|identity| {
                    merge_group(identity, &edges)
                        .pop_first()
                        .unwrap_or_else(|| identity.clone())
                })
                .clone();
            spans.entry(root).or_default().push((started, ended));
        }
        for intervals in spans.values_mut() {
            *intervals = union_intervals(std::mem::take(intervals));
        }
        Ok(spans)
    }
}

/// A category name as kept: trimmed, not empty, not too long.
fn category_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > CATEGORY_NAME_LIMIT {
        return Err(StorageError::Integrity("分类名称不能为空或过长".into()));
    }
    Ok(name.to_owned())
}

/// The same time as `intervals`, in order with overlaps joined.
fn union_intervals(mut intervals: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    intervals.sort_unstable();
    let mut joined: Vec<(i64, i64)> = Vec::with_capacity(intervals.len());
    for (started, ended) in intervals {
        match joined.last_mut() {
            Some(last) if started <= last.1 => last.1 = last.1.max(ended),
            _ => joined.push((started, ended)),
        }
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDITOR: &str = "path:c:\\private\\editor.exe";

    #[test]
    fn datasets_start_with_five_presets_in_their_own_colors() {
        let (_dir, s) = fixtures::storage();
        let categories = s.categories().unwrap();
        assert_eq!(
            categories
                .iter()
                .map(|c| c.preset.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["work", "study", "entertainment", "social", "tools"]
        );
        assert_eq!(
            categories.iter().map(|c| c.color).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert!(
            categories
                .iter()
                .all(|c| c.name.is_none() && c.folders.is_empty())
        );
        let added = s.add_category("  Games ").unwrap();
        let games = s.categories().unwrap().pop().unwrap();
        assert_eq!(
            (games.id, games.name.as_deref(), games.color),
            (added, Some("Games"), 5)
        );
        assert!(s.add_category("   ").is_err());
        assert!(
            s.add_category(&"x".repeat(CATEGORY_NAME_LIMIT + 1))
                .is_err()
        );
    }

    #[test]
    fn renaming_keeps_a_preset_able_to_take_its_own_name_back() {
        let (_dir, s) = fixtures::storage();
        let work = s.categories().unwrap()[0].id;
        s.rename_category(work, "Job").unwrap();
        assert_eq!(s.categories().unwrap()[0].name.as_deref(), Some("Job"));
        s.rename_category(work, " ").unwrap();
        assert_eq!(s.categories().unwrap()[0].name, None);
        let own = s.add_category("Mine").unwrap();
        assert!(s.rename_category(own, "").is_err());
    }

    #[test]
    fn a_choice_by_hand_wins_over_the_longest_matching_folder() {
        let (_dir, s) = fixtures::storage();
        let [work, study, play, ..] = ids(&s);
        let decided = |s: &Storage| s.application_category(EDITOR).unwrap();
        assert_eq!(decided(&s).category, None);
        // A folder only holds what lies inside it.
        s.add_category_folder(study, "C:/priv").unwrap();
        assert_eq!(decided(&s).category, None);
        s.add_category_folder(work, "C:\\").unwrap();
        s.add_category_folder(play, "c:\\Private\\").unwrap();
        assert_eq!(
            decided(&s),
            ApplicationCategory {
                category: Some(play),
                choice: CategoryChoice::Auto,
                folder: Some("c:\\private".into()),
            }
        );
        // One folder, one category.
        assert!(s.add_category_folder(work, "c:\\private").is_err());
        s.set_application_category(EDITOR, CategoryChoice::Manual(Some(study)))
            .unwrap();
        assert_eq!(decided(&s).category, Some(study));
        s.set_application_category(EDITOR, CategoryChoice::Manual(None))
            .unwrap();
        assert_eq!(decided(&s).category, None);
        assert_eq!(decided(&s).choice, CategoryChoice::Manual(None));
        s.set_application_category(EDITOR, CategoryChoice::Auto)
            .unwrap();
        assert_eq!(decided(&s).category, Some(play));
        s.remove_category_folder("C:\\Private").unwrap();
        assert_eq!(decided(&s).category, Some(work));
    }

    #[test]
    fn deleting_a_category_takes_its_folders_and_choices_with_it() {
        let (_dir, s) = fixtures::storage();
        let [work, study, ..] = ids(&s);
        s.add_category_folder(work, "c:\\private").unwrap();
        s.set_application_category(EDITOR, CategoryChoice::Manual(Some(study)))
            .unwrap();
        s.delete_category(study).unwrap();
        let decided = s.application_category(EDITOR).unwrap();
        assert_eq!(
            (decided.category, decided.choice),
            (Some(work), CategoryChoice::Auto)
        );
        s.delete_category(work).unwrap();
        assert_eq!(s.application_category(EDITOR).unwrap().category, None);
        assert_eq!(s.categories().unwrap().len(), 3);
    }

    #[test]
    fn merged_applications_share_a_category_and_deleted_history_forgets_it() {
        let (_dir, s) = fixtures::storage();
        let [work, study, ..] = ids(&s);
        let other = "path:c:\\other\\editor.exe";
        s.set_application_category(other, CategoryChoice::Manual(Some(work)))
            .unwrap();
        s.set_application_merge(EDITOR, other, true).unwrap();
        assert_eq!(s.application_category(EDITOR).unwrap().category, Some(work));
        s.set_application_category(EDITOR, CategoryChoice::Manual(Some(study)))
            .unwrap();
        assert_eq!(
            s.application_categories([EDITOR, other]).unwrap(),
            BTreeMap::from([(EDITOR.into(), Some(study)), (other.into(), Some(study))])
        );
        s.delete_application_history(EDITOR).unwrap();
        s.set_application_merge(EDITOR, other, false).unwrap();
        assert_eq!(s.application_category(EDITOR).unwrap().category, None);
    }

    #[test]
    fn focused_spans_are_clipped_joined_and_never_past_now() {
        let (_dir, s) = fixtures::storage();
        assert_eq!(
            s.focused_spans(2000, 5000).unwrap(),
            FocusSpans::from([(EDITOR.into(), vec![(2000, 5000)])])
        );
        assert_eq!(
            s.focused_spans(0, i64::MAX).unwrap(),
            FocusSpans::from([(EDITOR.into(), vec![(1000, 6000)])])
        );
        assert!(s.focused_spans(6000, 9000).unwrap().is_empty());
        assert_eq!(
            union_intervals(vec![(5, 9), (1, 3), (2, 4), (9, 10)]),
            [(1, 4), (5, 10)]
        );
    }

    #[test]
    fn deleting_a_category_sends_its_whole_merged_group_back_to_folders() {
        let (_dir, s) = fixtures::storage();
        let [work, study, play, social, _] = ids(&s);
        let other = "path:c:\\other\\editor.exe";
        let chose = |identity: &str, category: i64, at: i64| {
            s.connection
                .execute(
                    "INSERT OR REPLACE INTO application_category_assignments VALUES (?1, ?2, ?3)",
                    params![identity, category, at],
                )
                .unwrap();
        };
        // Each chose apart before they were merged; the later choice shows.
        chose(EDITOR, study, 1);
        chose(other, work, 2);
        s.set_application_merge(EDITOR, other, true).unwrap();
        s.add_category_folder(play, "c:\\private").unwrap();
        assert_eq!(s.application_category(EDITOR).unwrap().category, Some(work));
        s.delete_category(work).unwrap();
        let decided = s.application_category(EDITOR).unwrap();
        assert_eq!(
            (decided.category, decided.choice),
            (Some(play), CategoryChoice::Auto)
        );
        // Split again, neither brings back its older choice.
        s.set_application_merge(EDITOR, other, false).unwrap();
        assert_eq!(
            s.application_category(EDITOR).unwrap().choice,
            CategoryChoice::Auto
        );
        assert_eq!(
            s.application_category(other).unwrap().choice,
            CategoryChoice::Auto
        );
        // A group whose newer choice is another category keeps it.
        chose(EDITOR, study, 3);
        chose(other, social, 4);
        s.set_application_merge(EDITOR, other, true).unwrap();
        s.delete_category(study).unwrap();
        assert_eq!(
            s.application_category(EDITOR).unwrap().category,
            Some(social)
        );
    }

    #[test]
    fn categories_outlive_clearing_and_return_with_backups_and_recovery() {
        let (source, s) = fixtures::storage();
        let [work, study, play, ..] = ids(&s);
        s.rename_category(work, "Job").unwrap();
        s.add_category_folder(play, "c:\\private").unwrap();
        s.set_application_category(EDITOR, CategoryChoice::Manual(Some(work)))
            .unwrap();
        // A deleted preset stays deleted.
        s.delete_category(study).unwrap();
        let categories = s.categories().unwrap();
        let chosen = s.application_category(EDITOR).unwrap();

        let folder = tempfile::tempdir().unwrap();
        let archive = folder.path().join("portable.zip");
        s.export_backup(
            &archive,
            BackupOptions {
                include_snapshots: false,
                password: None,
            },
        )
        .unwrap();
        let target = tempfile::tempdir().unwrap();
        let target = Storage::open(target.path()).unwrap();
        let prepared = Storage::prepare_restore(&archive, None, folder.path()).unwrap();
        target.replace_from_backup(prepared, false).unwrap();
        assert_eq!(target.categories().unwrap(), categories);
        assert_eq!(target.application_category(EDITOR).unwrap(), chosen);

        s.checkpoint().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("recovered");
        drop(s);
        let (recovered, _) =
            Storage::recover_to_new_directory(source.path(), &destination).unwrap();
        assert_eq!(recovered.categories().unwrap(), categories);
        assert_eq!(recovered.application_category(EDITOR).unwrap(), chosen);

        recovered.clear_all().unwrap();
        assert_eq!(recovered.categories().unwrap(), categories);
        assert_eq!(recovered.application_category(EDITOR).unwrap(), chosen);
    }

    #[test]
    fn focused_spans_join_merged_applications_and_leave_windows_own_out() {
        let (_dir, s) = fixtures::storage();
        let other = "path:c:\\other\\editor.exe";
        let shell = "path:c:\\windows\\systemapps\\shell\\shellhost.exe";
        window(&s, 3, 2, other, 7000, 8000);
        window(&s, 5, 3, shell, 8000, 9000);
        s.set_application_merge(EDITOR, other, true).unwrap();
        // The group goes by the first of its identities.
        assert_eq!(
            s.focused_spans(0, 10_000).unwrap(),
            FocusSpans::from([(other.into(), vec![(1000, 6000), (7000, 8000)])])
        );
    }

    /// Record a focused window of `identity` open from `from` to `to`.
    fn window(s: &Storage, sequence: u64, id: u64, identity: &str, from: i64, to: i64) {
        use timelens_ipc::{WindowTransition, WindowTransitionKind};
        let path = identity.strip_prefix("path:").unwrap().to_owned();
        let event = |time: i64, kind: WindowTransitionKind| CollectorEvent {
            observed_at_utc_ms: time,
            monotonic_ms: time as u64,
            body: Some(collector_event::Body::WindowTransition(WindowTransition {
                kind: kind as i32,
                window: Some(WindowObservation {
                    window_id: id,
                    process_id: id as u32 + 10,
                    process_started_at_100ns: 3,
                    application_identity: identity.into(),
                    identity_source: IdentitySource::ExecutablePath as i32,
                    executable_path: Some(path.clone()),
                    app_user_model_id: None,
                    package_identity: None,
                    displayed: true,
                    focused: true,
                    on_current_virtual_desktop: Some(true),
                    virtual_desktop_id: None,
                }),
            })),
        };
        s.ingest_event_batch(&EventBatch {
            collector_run_id: vec![1; 16],
            first_sequence: sequence,
            events: vec![
                event(from, WindowTransitionKind::Opened),
                event(to, WindowTransitionKind::Closed),
            ],
        })
        .unwrap();
    }

    /// The presets' ids, in order.
    fn ids(s: &Storage) -> [i64; 5] {
        let categories = s.categories().unwrap();
        std::array::from_fn(|i| categories[i].id)
    }
}
