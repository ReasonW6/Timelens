CREATE TABLE collection_policy(id INTEGER PRIMARY KEY CHECK(id=1), policy_json TEXT NOT NULL) STRICT;
INSERT INTO collection_policy VALUES(1,'{"revision":0,"paused":false,"activity":[],"input":[]}');
CREATE TABLE application_merges(left_identity TEXT NOT NULL, right_identity TEXT NOT NULL, created_utc_ms INTEGER NOT NULL, prior_rules_json TEXT NOT NULL, PRIMARY KEY(left_identity,right_identity), CHECK(left_identity<right_identity)) STRICT;
CREATE TABLE system_intervals(interval_id INTEGER PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN ('desktop_idle','locked','sleep','global_pause','privacy_exclusion','session_disconnected','secure_desktop','clock_discontinuity','system_end')), started_utc_ms INTEGER NOT NULL, ended_utc_ms INTEGER NOT NULL, duration_ms INTEGER NOT NULL CHECK(duration_ms>=0), timezone_offset_minutes INTEGER NOT NULL, CHECK(ended_utc_ms>=started_utc_ms)) STRICT;
CREATE INDEX system_interval_range ON system_intervals(started_utc_ms,ended_utc_ms,kind);
ALTER TABLE data_availability RENAME TO availability_before_privacy;
CREATE TABLE data_availability (
    availability_id INTEGER PRIMARY KEY,
    data_class TEXT NOT NULL CHECK(data_class IN ('activity','input')),
    status TEXT NOT NULL CHECK(status IN ('monitoring_gap','cleaned')),
    started_utc_ms INTEGER NOT NULL, ended_utc_ms INTEGER NOT NULL,
    reason TEXT NOT NULL CHECK(reason IN ('collector_restart','buffer_overflow','input_overflow','retention_time','retention_space','user_deleted','clock_discontinuity')),
    item_count INTEGER, released_bytes INTEGER
) STRICT;
INSERT INTO data_availability SELECT * FROM availability_before_privacy;
DROP TABLE availability_before_privacy;
