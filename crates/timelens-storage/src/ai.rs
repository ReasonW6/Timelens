use super::*;
use chrono::{Local, NaiveDate};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use timelens_ai::{schedule::Schedule, *};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiSettings {
    pub days: Option<u32>,
    pub max_bytes: u64,
    pub versions_to_keep: usize,
    pub compression_enabled: bool,
    pub recent_messages: Option<usize>,
    pub notify_success: bool,
    pub notify_failure: bool,
}
impl Default for AiSettings {
    fn default() -> Self {
        Self {
            days: Some(30),
            max_bytes: 100 * 1024 * 1024,
            versions_to_keep: 3,
            compression_enabled: true,
            recent_messages: None,
            notify_success: false,
            notify_failure: true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AiJobKind {
    Summary,
    FollowUp {
        version_id: i64,
        user_message_id: i64,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AiJobSpec {
    pub kind: AiJobKind,
    pub provider: ProviderProfile,
    pub prompt: PromptPreset,
    pub effective_prompt: String,
    pub envelope: AiEnvelope,
    pub chunks: Vec<AiEnvelope>,
    pub retry_limit: u8,
}
#[derive(Clone, Debug)]
pub struct AiJob {
    pub id: i64,
    pub state: String,
    pub group_key: String,
    pub created_utc_ms: i64,
    pub attempts: u8,
    pub spec: AiJobSpec,
    pub body: String,
    pub reasoning: String,
    pub error: Option<AiFailure>,
    pub usage: TokenUsage,
}
#[derive(Clone, Debug)]
pub struct AiVersion {
    pub id: i64,
    pub job_id: i64,
    pub group_key: String,
    pub created_utc_ms: i64,
    pub pinned: bool,
    pub answer: String,
    pub reasoning: String,
    pub usage: TokenUsage,
    pub snapshot: AiJobSpec,
}
#[derive(Clone, Debug)]
pub struct AiMessage {
    pub id: i64,
    pub version_id: i64,
    pub parent_id: Option<i64>,
    pub role: String,
    pub body: String,
    pub reasoning: String,
    pub error: Option<AiFailure>,
    pub usage: TokenUsage,
    pub completed: bool,
}
#[derive(Clone, Debug)]
pub struct SnapshotConsent {
    pub slot_id: i64,
    pub pixel_hash: String,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct AiRetentionOutcome {
    pub deleted_jobs: u64,
    pub pinned_bytes: u64,
    pub over_limit: bool,
}

fn json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|_| StorageError::Integrity("AI 数据编码失败".into()))
}
fn decode<T: DeserializeOwned>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(|_| StorageError::Integrity("AI 数据格式或版本无效".into()))
}
fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::Integrity(message.into())
}

impl Storage {
    pub(crate) fn initialize_ai(&self) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO ai_prompts(id,preset_json) VALUES('default',?1)",
            [json(&PromptPreset::default())?],
        )?;
        self.connection.execute(
            "INSERT OR IGNORE INTO ai_settings(id,settings_json) VALUES(1,?1)",
            [json(&AiSettings::default())?],
        )?;
        // A crashed worker is never silently replayed; partial output remains reviewable.
        self.connection.execute(
            "UPDATE ai_jobs SET state='failed', error_json=?1 WHERE state='running'",
            [json(&AiFailure {
                status: None,
                message: "AI 工作进程中断；本地采集不受影响，可手动重试".into(),
                retry_after_seconds: None,
                retryable: false,
            })?],
        )?;
        Ok(())
    }
    pub fn ai_settings(&self) -> Result<AiSettings> {
        decode(&self.connection.query_row(
            "SELECT settings_json FROM ai_settings WHERE id=1",
            [],
            |r| r.get::<_, String>(0),
        )?)
    }
    pub fn set_ai_settings(&self, settings: &AiSettings) -> Result<()> {
        if settings.days.is_some_and(|n| n == 0 || n > 36500)
            || settings.max_bytes < 1024 * 1024
            || !(1..=100).contains(&settings.versions_to_keep)
            || settings.recent_messages == Some(0)
        {
            return Err(invalid("AI 保留或上下文设置无效"));
        }
        self.connection.execute(
            "UPDATE ai_settings SET settings_json=?1 WHERE id=1",
            [json(settings)?],
        )?;
        Ok(())
    }
    pub fn ai_profiles(&self) -> Result<Vec<ProviderProfile>> {
        self.read_json_list("SELECT config_json FROM ai_profiles ORDER BY rowid")
    }
    pub fn ai_profile(&self, id: &str) -> Result<ProviderProfile> {
        decode(&self.connection.query_row(
            "SELECT config_json FROM ai_profiles WHERE id=?1",
            [id],
            |r| r.get::<_, String>(0),
        )?)
    }
    pub fn save_ai_profile(&self, profile: &ProviderProfile) -> Result<()> {
        profile.validate().map_err(invalid)?;
        self.connection.execute("INSERT INTO ai_profiles(id,config_json) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET config_json=excluded.config_json", params![profile.id, json(profile)?])?;
        Ok(())
    }
    pub fn delete_ai_profile(&self, id: &str) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute("DELETE FROM ai_profiles WHERE id=?1", [id])?;
        self.pause_schedules_for(Some(id), None)?;
        tx.commit()?;
        Ok(())
    }
    pub fn ai_prompts(&self) -> Result<Vec<PromptPreset>> {
        self.read_json_list("SELECT preset_json FROM ai_prompts ORDER BY rowid")
    }
    pub fn save_ai_prompt(&self, prompt: &PromptPreset) -> Result<()> {
        if !valid_id(&prompt.id)
            || prompt.name.trim().is_empty()
            || prompt.text.trim().is_empty()
            || prompt.text.len() > 128 * 1024
            || prompt.language.len() > 100
        {
            return Err(invalid("提示词名称、正文或语言无效"));
        }
        self.connection.execute("INSERT INTO ai_prompts(id,preset_json) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET preset_json=excluded.preset_json", params![prompt.id, json(prompt)?])?;
        Ok(())
    }
    pub fn delete_ai_prompt(&self, id: &str) -> Result<()> {
        let tx = self.connection.unchecked_transaction()?;
        tx.execute("DELETE FROM ai_prompts WHERE id=?1", [id])?;
        self.pause_schedules_for(None, Some(id))?;
        tx.commit()?;
        Ok(())
    }
    fn pause_schedules_for(&self, provider: Option<&str>, prompt: Option<&str>) -> Result<()> {
        for mut schedule in self.ai_schedules()? {
            if provider == Some(schedule.provider_id.as_str())
                || prompt == Some(schedule.prompt_id.as_str())
            {
                schedule.enabled = false;
                schedule.paused_reason = Some("关联配置已删除，请重新选择".into());
                self.connection.execute(
                    "UPDATE ai_schedules SET schedule_json=?2 WHERE id=?1",
                    params![schedule.id, json(&schedule)?],
                )?;
            }
        }
        Ok(())
    }
    fn read_json_list<T: DeserializeOwned>(&self, sql: &str) -> Result<Vec<T>> {
        let mut statement = self.connection.prepare(sql)?;
        statement
            .query_map([], |r| r.get::<_, String>(0))?
            .map(|row| decode(&row?))
            .collect()
    }
    pub fn ai_schedules(&self) -> Result<Vec<Schedule>> {
        self.read_json_list("SELECT schedule_json FROM ai_schedules ORDER BY id")
    }
    pub fn save_ai_schedule(&self, schedule: &Schedule) -> Result<()> {
        if !valid_id(&schedule.id) || schedule.retries.is_some_and(|r| r > 10) {
            return Err(invalid("AI 计划无效"));
        }
        if schedule.enabled {
            let profile = self.ai_profile(&schedule.provider_id)?;
            if !profile.is_tested() || profile.model.id != schedule.model_id {
                return Err(invalid("请先测试计划所选提供商和模型"));
            }
            if !self
                .ai_prompts()?
                .iter()
                .any(|p| p.id == schedule.prompt_id)
            {
                return Err(invalid("计划提示词不存在"));
            }
        }
        self.connection.execute("INSERT INTO ai_schedules(id,schedule_json) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET schedule_json=excluded.schedule_json", params![schedule.id, json(schedule)?])?;
        Ok(())
    }

    pub fn ai_envelope(&self, start: i64, end: i64) -> Result<AiEnvelope> {
        if end <= start || start < 0 {
            return Err(invalid("总结范围无效"));
        }
        let timeline = self.timeline_snapshot(start, end)?;
        let (covered_ms, missing) = self.data_coverage(start, end)?;
        let applications = timeline
            .applications
            .iter()
            .map(|a| AiApplication {
                // A display-name fallback must not leak a path or internal identity.
                name: if a.display_name.contains(['\\', '/']) || a.display_name == a.identity {
                    "未知应用".into()
                } else {
                    a.display_name.clone()
                },
                opened_ms: a.opened_ms,
                displayed_ms: a.displayed_ms,
                focused_ms: a.focused_ms,
                background_ms: a.background_ms,
                window_count: a.window_count as u64,
            })
            .collect::<Vec<_>>();
        let input = InputTotals {
            keyboard: timeline.keyboard_count,
            left: timeline.left_click_count,
            middle: timeline.middle_click_count,
            right: timeline.right_click_count,
        };
        let mut hourly = vec![];
        let mut hour = self.connection.prepare("SELECT (minute_started_utc_ms/3600000)*3600000, SUM(keyboard_count), SUM(left_click_count), SUM(middle_click_count), SUM(right_click_count) FROM input_minute_buckets WHERE minute_started_utc_ms>=?1 AND minute_started_utc_ms<?2 GROUP BY 1 ORDER BY 1")?;
        let rows = hour.query_map(params![start, end], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                InputTotals {
                    keyboard: r.get::<_, i64>(1)? as u64,
                    left: r.get::<_, i64>(2)? as u64,
                    middle: r.get::<_, i64>(3)? as u64,
                    right: r.get::<_, i64>(4)? as u64,
                },
            ))
        })?;
        let mut hour_inputs = BTreeMap::new();
        for row in rows {
            let (h, i) = row?;
            hour_inputs.insert(h, i);
        }
        for a in &timeline.applications {
            for segment in a.segments.iter().filter(|s| s.focused) {
                let mut h = segment.started_utc_ms.div_euclid(3_600_000) * 3_600_000;
                while h < segment.ended_utc_ms {
                    hour_inputs.entry(h).or_default();
                    h += 3_600_000;
                }
            }
        }
        for (h, i) in hour_inputs {
            let intervals = timeline
                .applications
                .iter()
                .flat_map(|a| a.segments.iter())
                .filter(|s| s.focused)
                .map(|s| {
                    (
                        s.started_utc_ms.max(h).max(start),
                        s.ended_utc_ms.min(h + 3_600_000).min(end),
                    )
                })
                .filter(|(s, e)| e > s)
                .collect::<Vec<_>>();
            hourly.push(HourlyTrend {
                started_utc_ms: h,
                focused_ms: union_duration_ms(&intervals),
                input: i,
            });
        }
        let mut frequencies = BTreeMap::<u32, u64>::new();
        let mut keys = self.connection.prepare("SELECT local_date,timezone_offset_minutes,scan_code,key_count FROM daily_physical_key_frequency ORDER BY local_date")?;
        for row in keys.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, u32>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })? {
            let (date, offset, code, count) = row?;
            let date = NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                .map_err(|_| invalid("键位日期无效"))?;
            let day = date
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp_millis()
                - offset * 60_000;
            // Daily data cannot be narrowed to an arbitrary partial day without fabrication.
            if day >= start && day + 86_400_000 <= end {
                *frequencies.entry(code).or_default() += count as u64;
            }
        }
        let mut top_keys = frequencies
            .into_iter()
            .map(|(physical_position, count)| KeyFrequency {
                physical_position,
                count,
            })
            .collect::<Vec<_>>();
        top_keys.sort_by_key(|k| std::cmp::Reverse(k.count));
        top_keys.truncate(20);
        let available = covered_ms > 0
            || !applications.is_empty()
            || input.keyboard + input.left + input.middle + input.right > 0;
        Ok(AiEnvelope { schema_version: 1, started_utc_ms: start, ended_utc_ms: end, timezone: "UTC timestamps; recorded local-date offsets retained".into(), covered_ms, available, applications, system_ms: self.system_totals(start,end)?, missing, input, hourly, top_keys, key_frequency_scope: "Only complete recorded local dates inside this range; partial-day key positions are unavailable".into() })
    }

    pub fn data_coverage(&self, start: i64, end: i64) -> Result<(u64, Vec<MissingInterval>)> {
        let mut observed = self.connection.prepare("SELECT first_observed_utc_ms,last_observed_utc_ms FROM collector_runs WHERE first_observed_utc_ms<?2 AND last_observed_utc_ms>?1")?;
        let mut intervals = observed
            .query_map(params![start, end], |r| {
                Ok((r.get::<_, i64>(0)?.max(start), r.get::<_, i64>(1)?.min(end)))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        intervals.sort_unstable();
        let mut union = Vec::<(i64, i64)>::new();
        for (s, e) in intervals {
            if let Some(last) = union.last_mut()
                && s <= last.1
            {
                last.1 = last.1.max(e);
            } else {
                union.push((s, e));
            }
        }
        let mut missing = vec![];
        let mut cursor = start;
        for (s, e) in &union {
            if *s > cursor {
                missing.push(MissingInterval {
                    started_utc_ms: cursor,
                    ended_utc_ms: *s,
                    category: "activity".into(),
                    reason: "unobserved".into(),
                });
            }
            cursor = cursor.max(*e);
        }
        if cursor < end {
            missing.push(MissingInterval {
                started_utc_ms: cursor,
                ended_utc_ms: end,
                category: "activity".into(),
                reason: "unobserved".into(),
            });
        }
        let mut gaps = self.connection.prepare("SELECT data_class,started_utc_ms,ended_utc_ms,reason FROM data_availability WHERE started_utc_ms<?2 AND ended_utc_ms>?1")?;
        for row in gaps.query_map(params![start, end], |r| {
            Ok(MissingInterval {
                category: r.get(0)?,
                started_utc_ms: r.get::<_, i64>(1)?.max(start),
                ended_utc_ms: r.get::<_, i64>(2)?.min(end),
                reason: r.get(3)?,
            })
        })? {
            missing.push(row?);
        }
        let mut system=self.connection.prepare("SELECT kind,started_utc_ms,ended_utc_ms FROM system_intervals WHERE kind IN ('global_pause','privacy_exclusion','session_disconnected','secure_desktop','clock_discontinuity') AND started_utc_ms<?2 AND ended_utc_ms>?1")?;
        for row in system.query_map(params![start, end], |r| {
            Ok(MissingInterval {
                category: "activity".into(),
                reason: r.get(0)?,
                started_utc_ms: r.get::<_, i64>(1)?.max(start),
                ended_utc_ms: r.get::<_, i64>(2)?.min(end),
            })
        })? {
            missing.push(row?);
        }
        let uncovered = missing
            .iter()
            .filter(|g| g.category == "activity")
            .map(|g| (g.started_utc_ms, g.ended_utc_ms))
            .collect::<Vec<_>>();
        Ok((
            (end - start) as u64 - union_duration_ms(&uncovered).min((end - start) as u64),
            missing,
        ))
    }

    pub fn enqueue_ai_summary(
        &self,
        profile_id: &str,
        prompt_id: &str,
        start: i64,
        end: i64,
        consents: &[SnapshotConsent],
        now: i64,
    ) -> Result<i64> {
        let profile = self.ai_profile(profile_id)?;
        let prompt = self
            .ai_prompts()?
            .into_iter()
            .find(|p| p.id == prompt_id)
            .ok_or_else(|| invalid("提示词不存在"))?;
        let envelope = self.ai_envelope(start, end)?;
        let chunks = self.ai_request_chunks(&envelope, &profile, &prompt)?;
        let spec = AiJobSpec {
            kind: AiJobKind::Summary,
            effective_prompt: prompt.render(&envelope),
            envelope,
            chunks,
            retry_limit: profile.retries,
            provider: profile,
            prompt,
        };
        self.enqueue_ai_spec(spec, None, consents, now, false)
    }
    fn enqueue_ai_spec(
        &self,
        spec: AiJobSpec,
        schedule_id: Option<&str>,
        consents: &[SnapshotConsent],
        now: i64,
        missed: bool,
    ) -> Result<i64> {
        spec.provider.validate().map_err(invalid)?;
        if !missed && !spec.provider.is_tested() {
            return Err(invalid("提供商或模型已改变，请重新测试连接"));
        }
        if !consents.is_empty()
            && (schedule_id.is_some() || spec.provider.model.vision != Capability::Supported)
        {
            return Err(invalid("只有手动任务与已确认视觉模型可发送逐张授权的图片"));
        }
        let (start, end) = (spec.envelope.started_utc_ms, spec.envelope.ended_utc_ms);
        let group = format!("{}:{start}:{end}", schedule_id.unwrap_or("manual"));
        let state = if missed {
            "missed"
        } else if !spec.envelope.available && consents.is_empty() {
            "no_data"
        } else {
            "queued"
        };
        let tx = self.connection.unchecked_transaction()?;
        let inserted = tx.execute("INSERT OR IGNORE INTO ai_jobs(state,priority,schedule_id,group_key,started_utc_ms,ended_utc_ms,created_utc_ms,updated_utc_ms,next_attempt_utc_ms,spec_json,target_version) VALUES(?1,?2,?3,?4,?5,?6,?7,?7,?7,?8,?9)",params![state,if schedule_id.is_none(){0}else{1},schedule_id,group,start,end,now,json(&spec)?,match spec.kind { AiJobKind::Summary=>None,AiJobKind::FollowUp{version_id,..}=>Some(version_id) }])?;
        if inserted == 0 {
            let id=tx.query_row("SELECT job_id FROM ai_jobs WHERE schedule_id=?1 AND started_utc_ms=?2 AND ended_utc_ms=?3",params![schedule_id,start,end],|r|r.get(0))?;
            tx.commit()?;
            return Ok(id);
        }
        let id = tx.last_insert_rowid();
        for app in self.timeline_snapshot(start, end)?.applications {
            for identity in self.merged_members(&app.identity)? {
                tx.execute("INSERT OR IGNORE INTO ai_version_sources(job_id,application_identity) VALUES(?1,?2)",params![id,identity])?;
            }
        }
        for consent in consents {
            let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM snapshot_slots s JOIN snapshot_blobs b ON b.blob_id=s.blob_id WHERE s.slot_id=?1 AND hex(b.content_sha256)=upper(?2) AND s.result='success' AND s.slot_started_utc_ms>=?3 AND s.slot_started_utc_ms<?4)",params![consent.slot_id,consent.pixel_hash,start,end],|r|r.get(0))?;
            if !valid {
                return Err(invalid("快照已变化或被清理，请重新预览并授权"));
            }
            tx.execute("INSERT INTO ai_image_authorizations(job_id,slot_id,pixel_hash,authorized_utc_ms) VALUES(?1,?2,?3,?4)",params![id,consent.slot_id,consent.pixel_hash,now])?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn enqueue_due_ai_schedules(&self, now: i64) -> Result<usize> {
        let mut count = 0;
        for mut schedule in self
            .ai_schedules()?
            .into_iter()
            .filter(|s| s.enabled && s.next_due_utc_ms <= now)
        {
            let profile = self.ai_profile(&schedule.provider_id);
            let prompt = self
                .ai_prompts()?
                .into_iter()
                .find(|p| p.id == schedule.prompt_id);
            let Ok(profile) = profile else {
                schedule.enabled = false;
                schedule.paused_reason = Some("提供商已删除".into());
                self.save_ai_schedule(&schedule)?;
                continue;
            };
            if !profile.is_tested() || profile.model.id != schedule.model_id || prompt.is_none() {
                schedule.enabled = false;
                schedule.paused_reason = Some("配置、模型或提示词需要重新测试与选择".into());
                self.save_ai_schedule(&schedule)?;
                continue;
            }
            let prompt = prompt.unwrap();
            let windows = schedule.take_due(now, &Local).map_err(invalid)?;
            // Queue insertion is idempotent; cursor is only advanced after every due row persists.
            for range in windows {
                let envelope = if range.missed {
                    AiEnvelope {
                        schema_version: 1,
                        started_utc_ms: range.started_utc_ms,
                        ended_utc_ms: range.ended_utc_ms,
                        ..Default::default()
                    }
                } else {
                    self.ai_envelope(range.started_utc_ms, range.ended_utc_ms)?
                };
                let chunks = if range.missed {
                    vec![]
                } else {
                    self.ai_request_chunks(&envelope, &profile, &prompt)?
                };
                let spec = AiJobSpec {
                    kind: AiJobKind::Summary,
                    provider: profile.clone(),
                    prompt: prompt.clone(),
                    effective_prompt: prompt.render(&envelope),
                    envelope,
                    chunks,
                    retry_limit: schedule.retries.unwrap_or(profile.retries),
                };
                self.enqueue_ai_spec(spec, Some(&schedule.id), &[], now, range.missed)?;
                count += 1;
            }
            self.save_ai_schedule(&schedule)?;
        }
        Ok(count)
    }

    pub fn ai_request_chunks(
        &self,
        envelope: &AiEnvelope,
        profile: &ProviderProfile,
        prompt: &PromptPreset,
    ) -> Result<Vec<AiEnvelope>> {
        let budget = (profile.model.context_tokens as usize)
            .saturating_sub(profile.parameters.max_output_tokens.unwrap_or(4096) as usize)
            .saturating_sub(timelens_ai::context::estimated_tokens(
                &prompt.render(envelope),
            ))
            .saturating_sub(1000);
        if budget < 256 {
            return Err(invalid("提示词与最大输出占满模型上下文，请调整设置"));
        }
        if timelens_ai::context::estimated_tokens(&json(envelope)?) <= budget {
            return Ok(vec![]);
        }
        let mut chunks = vec![];
        for (start, end) in timelens_ai::schedule::natural_day_ranges(
            envelope.started_utc_ms,
            envelope.ended_utc_ms,
            &Local,
        )
        .map_err(invalid)?
        {
            let day = self.ai_envelope(start, end)?;
            if !day.available {
                continue;
            }
            if timelens_ai::context::estimated_tokens(&json(&day)?) <= budget {
                chunks.push(day);
                continue;
            }
            let mut current = day.clone();
            current.applications.clear();
            if timelens_ai::context::estimated_tokens(&json(&current)?) > budget {
                return Err(invalid("单日聚合超出此模型上下文，请选择更大上下文模型"));
            }
            for application in day.applications {
                current.applications.push(application);
                if timelens_ai::context::estimated_tokens(&json(&current)?) > budget {
                    let last = current.applications.pop().unwrap();
                    if current.applications.is_empty() {
                        return Err(invalid("单个应用数据超出上下文，未静默丢弃"));
                    }
                    chunks.push(current.clone());
                    current.applications = vec![last];
                }
            }
            if !current.applications.is_empty() {
                chunks.push(current);
            }
        }
        Ok(chunks)
    }

    pub fn ai_jobs(&self, limit: u32) -> Result<Vec<AiJob>> {
        let mut stmt = self.connection.prepare(
            "SELECT job_id FROM ai_jobs ORDER BY created_utc_ms DESC,job_id DESC LIMIT ?1",
        )?;
        let ids = stmt
            .query_map([limit.min(1000)], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter().map(|id| self.ai_job(id)).collect()
    }
    pub fn ai_job(&self, id: i64) -> Result<AiJob> {
        let row=self.connection.query_row("SELECT state,group_key,created_utc_ms,attempts,spec_json,partial_body,partial_reasoning,error_json,usage_json FROM ai_jobs WHERE job_id=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,u8>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,String>(8)?)))?;
        Ok(AiJob {
            id,
            state: row.0,
            group_key: row.1,
            created_utc_ms: row.2,
            attempts: row.3,
            spec: decode(&row.4)?,
            body: row.5,
            reasoning: row.6,
            error: row.7.map(|s| decode(&s)).transpose()?,
            usage: decode(&row.8)?,
        })
    }
    pub fn claim_ai_job(&self, now: i64) -> Result<Option<AiJob>> {
        let tx = self.connection.unchecked_transaction()?;
        let id=tx.query_row("SELECT job_id FROM ai_jobs WHERE state='queued' AND next_attempt_utc_ms<=?1 AND NOT EXISTS(SELECT 1 FROM ai_jobs WHERE state='running') ORDER BY priority,created_utc_ms,job_id LIMIT 1",[now],|r|r.get::<_,i64>(0)).optional()?;
        if let Some(id) = id {
            tx.execute("UPDATE ai_jobs SET state='running',attempts=attempts+1,updated_utc_ms=?2,error_json=NULL WHERE job_id=?1",params![id,now])?;
        }
        tx.commit()?;
        id.map(|id| self.ai_job(id)).transpose()
    }
    pub fn persist_ai_progress(
        &self,
        id: i64,
        body: &str,
        reasoning: &str,
        usage: &TokenUsage,
        now: i64,
    ) -> Result<()> {
        if body.len() + reasoning.len() > MAX_OUTPUT_BYTES {
            return Err(invalid("AI 回答超过本地大小限制"));
        }
        self.connection.execute("UPDATE ai_jobs SET partial_body=?2,partial_reasoning=?3,usage_json=?4,updated_utc_ms=?5 WHERE job_id=?1 AND state='running'",params![id,body,reasoning,json(usage)?,now])?;
        Ok(())
    }
    pub fn fail_ai_job(&self, id: i64, failure: &AiFailure, now: i64) -> Result<()> {
        let job = self.ai_job(id)?;
        let image_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM ai_image_authorizations WHERE job_id=?1",
            [id],
            |r| r.get(0),
        )?;
        let delay = retry_delay(
            job.attempts.saturating_sub(1),
            job.spec.retry_limit,
            !job.body.is_empty(),
            image_count > 0,
            failure,
        );
        self.connection.execute("UPDATE ai_jobs SET state=?2,error_json=?3,next_attempt_utc_ms=?4,updated_utc_ms=?5 WHERE job_id=?1 AND state='running'",params![id,if delay.is_some(){"queued"}else{"failed"},json(failure)?,now.saturating_add(delay.unwrap_or(0).saturating_mul(1000).min(i64::MAX as u64) as i64),now])?;
        if delay.is_none()
            && let AiJobKind::FollowUp {
                version_id,
                user_message_id,
            } = job.spec.kind
        {
            self.connection.execute("INSERT INTO ai_messages(version_id,parent_id,role,body,reasoning,error_json,usage_json,created_utc_ms,completed) VALUES(?1,?2,'assistant',?3,?4,?5,?6,?7,0)",params![version_id,user_message_id,job.body,job.reasoning,json(failure)?,json(&job.usage)?,now])?;
        }
        Ok(())
    }
    pub fn cancel_ai_job(&self, id: i64, now: i64) -> Result<()> {
        self.connection.execute("UPDATE ai_jobs SET state='canceled',updated_utc_ms=?2 WHERE job_id=?1 AND state IN ('queued','running')",params![id,now])?;
        Ok(())
    }
    pub fn finish_ai_job(&self, id: i64, now: i64) -> Result<i64> {
        let job = self.ai_job(id)?;
        if job.state != "running" || job.body.trim().is_empty() {
            return Err(invalid("AI 任务未运行或没有有效回答"));
        }
        let tx = self.connection.unchecked_transaction()?;
        let version = match job.spec.kind {
            AiJobKind::Summary => {
                tx.execute("INSERT INTO ai_versions(job_id,group_key,created_utc_ms,answer,reasoning,usage_json,snapshot_json) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![id,job.group_key,now,job.body,job.reasoning,json(&job.usage)?,json(&job.spec)?])?;
                tx.last_insert_rowid()
            }
            AiJobKind::FollowUp {
                version_id,
                user_message_id,
            } => {
                tx.execute("INSERT INTO ai_messages(version_id,parent_id,role,body,reasoning,usage_json,created_utc_ms) VALUES(?1,?2,'assistant',?3,?4,?5,?6)",params![version_id,user_message_id,job.body,job.reasoning,json(&job.usage)?,now])?;
                version_id
            }
        };
        tx.execute("UPDATE ai_jobs SET state='completed',partial_body='',partial_reasoning='',updated_utc_ms=?2 WHERE job_id=?1",params![id,now])?;
        tx.commit()?;
        self.apply_ai_retention(now)?;
        Ok(version)
    }
    pub fn consume_ai_images(&self, id: i64, now: i64) -> Result<Vec<ImageInput>> {
        use base64::Engine;
        let tx = self.connection.unchecked_transaction()?;
        let mut stmt=tx.prepare("SELECT slot_id,pixel_hash,consumed_utc_ms FROM ai_image_authorizations WHERE job_id=?1 ORDER BY slot_id")?;
        let rows = stmt
            .query_map([id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut images = vec![];
        for (slot, hash, consumed) in rows {
            if consumed.is_some() {
                return Err(invalid("图片的单次授权已使用，需重新预览授权"));
            }
            let image = self.load_snapshot_image(slot)?;
            if !self
                .snapshot_content_hash(slot)?
                .eq_ignore_ascii_case(&hash)
            {
                return Err(invalid("快照内容已改变"));
            }
            images.push(ImageInput {
                webp_base64: base64::engine::general_purpose::STANDARD.encode(&image.webp),
                captured_utc_ms: image
                    .slot
                    .captured_at_utc_ms
                    .unwrap_or(image.slot.slot_started_utc_ms),
            });
        }
        tx.execute("UPDATE ai_image_authorizations SET consumed_utc_ms=?2 WHERE job_id=?1 AND consumed_utc_ms IS NULL",params![id,now])?;
        tx.commit()?;
        Ok(images)
    }
    pub fn snapshot_content_hash(&self, slot: i64) -> Result<String> {
        self.connection.query_row("SELECT hex(b.content_sha256) FROM snapshot_slots s JOIN snapshot_blobs b ON b.blob_id=s.blob_id WHERE s.slot_id=?1 AND s.result='success'",[slot],|r|r.get(0)).map_err(Into::into)
    }

    pub fn ai_versions(&self) -> Result<Vec<AiVersion>> {
        let mut s = self.connection.prepare(
            "SELECT version_id FROM ai_versions ORDER BY created_utc_ms DESC,version_id DESC",
        )?;
        let ids = s
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.into_iter().map(|id| self.ai_version(id)).collect()
    }
    pub fn ai_version(&self, id: i64) -> Result<AiVersion> {
        let r=self.connection.query_row("SELECT job_id,group_key,created_utc_ms,pinned,answer,reasoning,usage_json,snapshot_json FROM ai_versions WHERE version_id=?1",[id],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,bool>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?)))?;
        Ok(AiVersion {
            id,
            job_id: r.0,
            group_key: r.1,
            created_utc_ms: r.2,
            pinned: r.3,
            answer: r.4,
            reasoning: r.5,
            usage: decode(&r.6)?,
            snapshot: decode(&r.7)?,
        })
    }
    pub fn pin_ai_version(&self, id: i64, pinned: bool) -> Result<()> {
        self.connection.execute(
            "UPDATE ai_versions SET pinned=?2 WHERE version_id=?1",
            params![id, pinned],
        )?;
        Ok(())
    }
    pub fn delete_ai_version(&self, id: i64) -> Result<()> {
        self.connection.execute(
            "DELETE FROM ai_jobs WHERE job_id=(SELECT job_id FROM ai_versions WHERE version_id=?1)",
            [id],
        )?;
        Ok(())
    }
    pub fn ai_messages(&self, version: i64) -> Result<Vec<AiMessage>> {
        let mut s=self.connection.prepare("SELECT message_id,parent_id,role,body,reasoning,error_json,usage_json,completed FROM ai_messages WHERE version_id=?1 ORDER BY message_id")?;
        s.query_map([version], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, bool>(7)?,
            ))
        })?
        .map(|row| {
            let r = row?;
            Ok(AiMessage {
                id: r.0,
                version_id: version,
                parent_id: r.1,
                role: r.2,
                body: r.3,
                reasoning: r.4,
                error: r.5.map(|s| decode(&s)).transpose()?,
                usage: decode(&r.6)?,
                completed: r.7,
            })
        })
        .collect()
    }
    pub fn ai_branch(&self, version: i64, leaf: Option<i64>) -> Result<Vec<AiMessage>> {
        let messages = self.ai_messages(version)?;
        let map = messages
            .into_iter()
            .map(|m| (m.id, m))
            .collect::<BTreeMap<_, _>>();
        let mut branch = vec![];
        let mut cursor = leaf;
        while let Some(id) = cursor {
            let m = map
                .get(&id)
                .ok_or_else(|| invalid("消息不属于当前版本分支"))?;
            branch.push(m.clone());
            cursor = m.parent_id;
            if branch.len() > map.len() {
                return Err(invalid("对话树出现环"));
            }
        }
        branch.reverse();
        Ok(branch)
    }
    pub fn enqueue_ai_followup(
        &self,
        version: i64,
        parent: Option<i64>,
        question: &str,
        now: i64,
    ) -> Result<i64> {
        if question.trim().is_empty() || question.len() > 128 * 1024 {
            return Err(invalid("问题为空或超过大小限制"));
        }
        self.ai_branch(version, parent)?;
        let mut spec = self.ai_version(version)?.snapshot;
        // Current credentials may be used only after this exact endpoint/model is tested again.
        let current = self.ai_profile(&spec.provider.id)?;
        if current.revision() != spec.provider.revision() || !current.is_tested() {
            return Err(invalid("当前配置已改变，请用当前配置重新生成总结后继续"));
        }
        self.connection.execute("INSERT INTO ai_messages(version_id,parent_id,role,body,created_utc_ms) VALUES(?1,?2,'user',?3,?4)",params![version,parent,question,now])?;
        let message = self.connection.last_insert_rowid();
        spec.kind = AiJobKind::FollowUp {
            version_id: version,
            user_message_id: message,
        };
        spec.envelope.available = true;
        match self.enqueue_ai_spec(spec, None, &[], now, false) {
            Ok(id) => Ok(id),
            Err(error) => {
                self.connection
                    .execute("DELETE FROM ai_messages WHERE message_id=?1", [message])?;
                Err(error)
            }
        }
    }
    pub fn mark_ai_context_cleared(
        &self,
        version: i64,
        parent: Option<i64>,
        now: i64,
    ) -> Result<i64> {
        self.ai_branch(version, parent)?;
        self.connection.execute("INSERT INTO ai_messages(version_id,parent_id,role,body,created_utc_ms) VALUES(?1,?2,'clear_context','',?3)",params![version,parent,now])?;
        Ok(self.connection.last_insert_rowid())
    }
    pub fn clear_ai_conversation(&self, version: i64) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.connection,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        tx.execute("DELETE FROM ai_jobs WHERE target_version=?1", [version])?;
        tx.execute("DELETE FROM ai_messages WHERE version_id=?1", [version])?;
        tx.commit()?;
        Ok(())
    }
    pub fn save_ai_compression(
        &self,
        version: i64,
        through: i64,
        summary: &str,
        usage: &TokenUsage,
        now: i64,
    ) -> Result<()> {
        self.ai_branch(version, Some(through))?;
        self.connection.execute("INSERT INTO ai_compressions(version_id,through_message_id,summary,usage_json,created_utc_ms) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(version_id,through_message_id) DO UPDATE SET summary=excluded.summary,usage_json=excluded.usage_json,created_utc_ms=excluded.created_utc_ms",params![version,through,summary,json(usage)?,now])?;
        Ok(())
    }
    pub fn ai_compression(
        &self,
        version: i64,
        branch: &[AiMessage],
    ) -> Result<Option<(i64, String)>> {
        for message in branch.iter().rev() {
            if let Some(summary)=self.connection.query_row("SELECT summary FROM ai_compressions WHERE version_id=?1 AND through_message_id=?2",params![version,message.id],|r|r.get::<_,String>(0)).optional()?{return Ok(Some((message.id,summary)));}
        }
        Ok(None)
    }
    pub fn ai_source_available(&self, version: i64) -> Result<bool> {
        let v = self.ai_version(version)?;
        let (covered, _) = self.data_coverage(
            v.snapshot.envelope.started_utc_ms,
            v.snapshot.envelope.ended_utc_ms,
        )?;
        Ok(covered >= v.snapshot.envelope.covered_ms && covered > 0)
    }

    pub fn apply_ai_retention(&self, now: i64) -> Result<AiRetentionOutcome> {
        let settings = self.ai_settings()?;
        let mut result = AiRetentionOutcome::default();
        let mut groups = BTreeMap::<String, usize>::new();
        for version in self.ai_versions()? {
            if version.pinned {
                continue;
            }
            let count = groups.entry(version.group_key.clone()).or_default();
            *count += 1;
            if *count > settings.versions_to_keep
                || settings.days.is_some_and(|days| {
                    version.created_utc_ms < now.saturating_sub(i64::from(days) * 86_400_000)
                })
            {
                self.delete_ai_version(version.id)?;
                result.deleted_jobs += 1;
            }
        }
        if let Some(days) = settings.days {
            result.deleted_jobs+=self.connection.execute("DELETE FROM ai_jobs WHERE state NOT IN ('running','queued') AND created_utc_ms<?1 AND NOT EXISTS(SELECT 1 FROM ai_versions WHERE ai_versions.job_id=ai_jobs.job_id)",[now.saturating_sub(i64::from(days)*86_400_000)])? as u64;
        }
        while self.ai_stored_bytes()? > settings.max_bytes {
            let id=self.connection.query_row("SELECT j.job_id FROM ai_jobs j LEFT JOIN ai_versions v ON v.job_id=j.job_id WHERE j.state NOT IN ('running','queued') AND COALESCE(v.pinned,0)=0 AND NOT EXISTS(SELECT 1 FROM ai_versions pinned WHERE pinned.version_id=j.target_version AND pinned.pinned=1) ORDER BY j.created_utc_ms LIMIT 1",[],|r|r.get::<_,i64>(0)).optional()?;
            let Some(id) = id else {
                break;
            };
            self.connection
                .execute("DELETE FROM ai_jobs WHERE job_id=?1", [id])?;
            result.deleted_jobs += 1;
        }
        let bytes = self.ai_stored_bytes()?;
        result.over_limit = bytes > settings.max_bytes;
        result.pinned_bytes=self.connection.query_row("SELECT COALESCE(SUM(length(answer)+length(reasoning)+length(snapshot_json)),0) FROM ai_versions WHERE pinned=1",[],|r|r.get::<_,i64>(0))? as u64;
        Ok(result)
    }
    fn ai_stored_bytes(&self) -> Result<u64> {
        let bytes:i64=self.connection.query_row("SELECT (SELECT COALESCE(SUM(length(CAST(spec_json AS BLOB))+length(CAST(partial_body AS BLOB))+length(CAST(partial_reasoning AS BLOB))+COALESCE(length(error_json),0)),0) FROM ai_jobs)+(SELECT COALESCE(SUM(length(CAST(answer AS BLOB))+length(CAST(reasoning AS BLOB))+length(CAST(snapshot_json AS BLOB))),0) FROM ai_versions)+(SELECT COALESCE(SUM(length(CAST(body AS BLOB))+length(CAST(reasoning AS BLOB))),0) FROM ai_messages)+(SELECT COALESCE(SUM(length(CAST(summary AS BLOB))),0) FROM ai_compressions)",[],|r|r.get(0))?;
        Ok(bytes as u64)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use timelens_ipc::{WindowTransition, WindowTransitionKind};
    pub(crate) fn fixture() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).unwrap();
        let mut profile = ProviderProfile::preset(0, "fixture".into());
        profile.model = Model::unknown("fixture-model");
        profile.tested_revision = Some(profile.revision());
        storage.save_ai_profile(&profile).unwrap();
        let event = |time, kind| CollectorEvent {
            observed_at_utc_ms: time,
            monotonic_ms: time as u64,
            body: Some(collector_event::Body::WindowTransition(WindowTransition {
                kind: kind as i32,
                window: Some(WindowObservation {
                    window_id: 1,
                    process_id: 2,
                    process_started_at_100ns: 3,
                    application_identity: "path:c:\\private\\editor.exe".into(),
                    identity_source: IdentitySource::ExecutablePath as i32,
                    executable_path: Some("C:\\private\\editor.exe".into()),
                    app_user_model_id: None,
                    package_identity: None,
                    displayed: true,
                    focused: true,
                    on_current_virtual_desktop: Some(true),
                    virtual_desktop_id: None,
                }),
            })),
        };
        storage
            .ingest_event_batch(&EventBatch {
                collector_run_id: vec![1; 16],
                first_sequence: 1,
                events: vec![
                    event(1000, WindowTransitionKind::Opened),
                    event(6000, WindowTransitionKind::Closed),
                ],
            })
            .unwrap();
        (dir, storage)
    }
    pub(crate) fn summary(storage: &Storage, now: i64) -> i64 {
        let job = storage
            .enqueue_ai_summary("fixture", "default", 1000, 6000, &[], now)
            .unwrap();
        storage.claim_ai_job(now).unwrap().unwrap();
        storage
            .persist_ai_progress(
                job,
                "answer-private-marker",
                "public reasoning",
                &TokenUsage {
                    input: Some(10),
                    output: Some(4),
                    ..Default::default()
                },
                now,
            )
            .unwrap();
        storage.finish_ai_job(job, now).unwrap()
    }
    #[test]
    fn empty_and_cleaned_ranges_never_claim_full_coverage_or_call_ai() {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path()).unwrap();
        let report = s.generate_local_report(1000, 6000, 7000).unwrap();
        assert_eq!(report.covered_ms, 0);
        assert_eq!(report.gaps[0].reason, "unobserved");
        let (_dir, s) = fixture();
        let env = s.ai_envelope(1, 10000).unwrap();
        assert_eq!(env.covered_ms, 5000);
        let wire = json(&env).unwrap();
        assert!(!wire.contains("private\\"));
        assert!(!wire.contains("path:"));
        assert!(!wire.contains("identity"));
        assert!(wire.contains("editor"));
        s.connection.execute("INSERT INTO data_availability(data_class,status,started_utc_ms,ended_utc_ms,reason) VALUES('activity','cleaned',1000,6000,'retention_time')",[]).unwrap();
        assert_eq!(s.data_coverage(1000, 6000).unwrap().0, 0);
    }
    #[test]
    fn queue_prioritizes_manual_deduplicates_plans_and_claims_only_one_worker() {
        let (_dir, s) = fixture();
        let mut schedule = Schedule {
            id: "hourly".into(),
            enabled: false,
            provider_id: "fixture".into(),
            model_id: "fixture-model".into(),
            prompt_id: "default".into(),
            kind: timelens_ai::schedule::ScheduleKind::Interval {
                hours: 1,
                anchor_utc_ms: 0,
            },
            next_due_utc_ms: 0,
            retries: None,
            paused_reason: None,
        };
        schedule.enable(1000, &Local).unwrap();
        s.save_ai_schedule(&schedule).unwrap();
        s.enqueue_due_ai_schedules(3_601_000).unwrap();
        let manual = s
            .enqueue_ai_summary("fixture", "default", 1000, 6000, &[], 3_602_000)
            .unwrap();
        assert_eq!(s.claim_ai_job(3_602_000).unwrap().unwrap().id, manual);
        assert!(s.claim_ai_job(3_602_000).unwrap().is_none());
        assert_eq!(s.enqueue_due_ai_schedules(3_602_000).unwrap(), 0);
        s.cancel_ai_job(manual, 3_603_000).unwrap();
        assert!(s.finish_ai_job(manual, 3_603_000).is_err());
    }
    #[test]
    fn regenerated_versions_have_independent_branches_and_pinned_versions_survive() {
        let (_dir, s) = fixture();
        let first = summary(&s, 7000);
        s.pin_ai_version(first, true).unwrap();
        let question = s.enqueue_ai_followup(first, None, "why?", 7100).unwrap();
        let claimed = s.claim_ai_job(7100).unwrap().unwrap();
        assert_eq!(claimed.id, question);
        s.persist_ai_progress(question, "follow-up", "", &TokenUsage::default(), 7200)
            .unwrap();
        s.finish_ai_job(question, 7200).unwrap();
        let second = summary(&s, 8000);
        assert!(s.ai_messages(second).unwrap().is_empty());
        let messages = s.ai_messages(first).unwrap();
        assert_eq!(messages.len(), 2);
        assert!(s.ai_branch(second, Some(messages[0].id)).is_err());
        summary(&s, 9000);
        summary(&s, 10000);
        summary(&s, 11000);
        assert_eq!(s.ai_versions().unwrap().len(), 4);
        assert!(s.ai_version(first).unwrap().pinned);
        s.clear_ai_conversation(first).unwrap();
        assert!(s.ai_messages(first).unwrap().is_empty());
        assert_eq!(s.ai_version(first).unwrap().answer, "answer-private-marker");
    }
    #[test]
    fn crashes_preserve_partial_answer_but_never_silently_replay_it() {
        let (dir, s) = fixture();
        let id = s
            .enqueue_ai_summary("fixture", "default", 1000, 6000, &[], 7000)
            .unwrap();
        s.claim_ai_job(7000).unwrap();
        s.persist_ai_progress(id, "partial", "exposed", &TokenUsage::default(), 7100)
            .unwrap();
        s.checkpoint().unwrap();
        drop(s);
        let s = Storage::open(dir.path()).unwrap();
        let job = s.ai_job(id).unwrap();
        assert_eq!(job.state, "failed");
        assert_eq!(job.body, "partial");
        assert!(s.claim_ai_job(7200).unwrap().is_none());
        let bytes = fs::read(s.database_path()).unwrap();
        assert!(!bytes.windows(7).any(|w| w == b"partial"));
    }
    #[test]
    fn changed_model_requires_retest_and_deletion_pauses_referencing_schedule() {
        let (_dir, s) = fixture();
        let mut p = s.ai_profile("fixture").unwrap();
        p.model.id = "changed".into();
        s.save_ai_profile(&p).unwrap();
        assert!(
            s.enqueue_ai_summary("fixture", "default", 1000, 6000, &[], 7000)
                .is_err()
        );
        p.tested_revision = Some(p.revision());
        s.save_ai_profile(&p).unwrap();
        let mut schedule = Schedule {
            id: "daily".into(),
            enabled: false,
            provider_id: p.id,
            model_id: p.model.id,
            prompt_id: "default".into(),
            kind: timelens_ai::schedule::ScheduleKind::Daily {
                hour: 22,
                minute: 0,
            },
            next_due_utc_ms: 0,
            retries: None,
            paused_reason: None,
        };
        schedule.enable(7000, &Local).unwrap();
        s.save_ai_schedule(&schedule).unwrap();
        s.delete_ai_profile("fixture").unwrap();
        assert!(!s.ai_schedules().unwrap()[0].enabled);
    }
}
