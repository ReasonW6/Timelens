CREATE TABLE ai_profiles (id TEXT PRIMARY KEY, config_json TEXT NOT NULL) STRICT;
CREATE TABLE ai_prompts (id TEXT PRIMARY KEY, preset_json TEXT NOT NULL) STRICT;
CREATE TABLE ai_settings (id INTEGER PRIMARY KEY CHECK(id=1), settings_json TEXT NOT NULL) STRICT;
CREATE TABLE ai_schedules (id TEXT PRIMARY KEY, schedule_json TEXT NOT NULL) STRICT;
CREATE TABLE ai_jobs (
    job_id INTEGER PRIMARY KEY, state TEXT NOT NULL CHECK(state IN ('queued','running','completed','no_data','missed','failed','canceled')),
    priority INTEGER NOT NULL, schedule_id TEXT, group_key TEXT NOT NULL,
    started_utc_ms INTEGER NOT NULL, ended_utc_ms INTEGER NOT NULL CHECK(ended_utc_ms > started_utc_ms),
    created_utc_ms INTEGER NOT NULL, updated_utc_ms INTEGER NOT NULL, next_attempt_utc_ms INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0, spec_json TEXT NOT NULL,
    partial_body TEXT NOT NULL DEFAULT '', partial_reasoning TEXT NOT NULL DEFAULT '', error_json TEXT,
    usage_json TEXT NOT NULL DEFAULT '{}', target_version INTEGER REFERENCES ai_versions(version_id) ON DELETE CASCADE,
    UNIQUE(schedule_id, started_utc_ms, ended_utc_ms)
) STRICT;
CREATE INDEX ai_queue_order ON ai_jobs(state, next_attempt_utc_ms, priority, created_utc_ms);
CREATE TABLE ai_versions (
    version_id INTEGER PRIMARY KEY, job_id INTEGER NOT NULL UNIQUE REFERENCES ai_jobs(job_id) ON DELETE CASCADE,
    group_key TEXT NOT NULL, created_utc_ms INTEGER NOT NULL, pinned INTEGER NOT NULL DEFAULT 0 CHECK(pinned IN (0,1)),
    answer TEXT NOT NULL, reasoning TEXT NOT NULL, usage_json TEXT NOT NULL, snapshot_json TEXT NOT NULL
) STRICT;
CREATE TABLE ai_messages (
    message_id INTEGER PRIMARY KEY, version_id INTEGER NOT NULL REFERENCES ai_versions(version_id) ON DELETE CASCADE,
    parent_id INTEGER REFERENCES ai_messages(message_id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK(role IN ('user','assistant','clear_context')),
    body TEXT NOT NULL, reasoning TEXT NOT NULL DEFAULT '', error_json TEXT, usage_json TEXT NOT NULL DEFAULT '{}',
    created_utc_ms INTEGER NOT NULL, completed INTEGER NOT NULL DEFAULT 1 CHECK(completed IN (0,1))
) STRICT;
CREATE INDEX ai_message_branch ON ai_messages(version_id, parent_id);
CREATE TABLE ai_compressions (
    version_id INTEGER NOT NULL REFERENCES ai_versions(version_id) ON DELETE CASCADE,
    through_message_id INTEGER NOT NULL REFERENCES ai_messages(message_id) ON DELETE CASCADE,
    summary TEXT NOT NULL, usage_json TEXT NOT NULL, created_utc_ms INTEGER NOT NULL,
    PRIMARY KEY(version_id, through_message_id)
) STRICT;
CREATE TABLE ai_version_sources (
    job_id INTEGER NOT NULL REFERENCES ai_jobs(job_id) ON DELETE CASCADE,
    application_identity TEXT NOT NULL, PRIMARY KEY(job_id, application_identity)
) STRICT;
CREATE TABLE ai_image_authorizations (
    job_id INTEGER NOT NULL REFERENCES ai_jobs(job_id) ON DELETE CASCADE,
    slot_id INTEGER NOT NULL, pixel_hash TEXT NOT NULL,
    authorized_utc_ms INTEGER NOT NULL, consumed_utc_ms INTEGER,
    PRIMARY KEY(job_id, slot_id)
) STRICT;
