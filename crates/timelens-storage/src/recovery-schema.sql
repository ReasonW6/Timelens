CREATE TABLE storage_checks(id INTEGER PRIMARY KEY CHECK(id=1), last_full_check_utc_ms INTEGER NOT NULL) STRICT;
INSERT INTO storage_checks VALUES(1,0);
ALTER TABLE data_availability RENAME TO availability_before_recovery;
CREATE TABLE data_availability (
    availability_id INTEGER PRIMARY KEY,
    data_class TEXT NOT NULL CHECK(data_class IN ('activity','input')),
    status TEXT NOT NULL CHECK(status IN ('monitoring_gap','cleaned')),
    started_utc_ms INTEGER NOT NULL, ended_utc_ms INTEGER NOT NULL,
    reason TEXT NOT NULL CHECK(reason IN ('collector_restart','buffer_overflow','input_overflow','retention_time','retention_space','user_deleted','clock_discontinuity','recovery_corruption')),
    item_count INTEGER, released_bytes INTEGER
) STRICT;
INSERT INTO data_availability SELECT * FROM availability_before_recovery;
DROP TABLE availability_before_recovery;

CREATE TABLE snapshot_slots_recovery (
    slot_id INTEGER PRIMARY KEY,
    slot_started_utc_ms INTEGER NOT NULL,
    captured_at_utc_ms INTEGER,
    display_key TEXT NOT NULL,
    display_x INTEGER NOT NULL,
    display_y INTEGER NOT NULL,
    display_width INTEGER NOT NULL CHECK(display_width>=0),
    display_height INTEGER NOT NULL CHECK(display_height>=0),
    orientation_degrees INTEGER NOT NULL CHECK(orientation_degrees IN (0,90,180,270)),
    pixel_width INTEGER NOT NULL CHECK(pixel_width>=0),
    pixel_height INTEGER NOT NULL CHECK(pixel_height>=0),
    blob_id INTEGER REFERENCES snapshot_blobs(blob_id),
    trigger TEXT NOT NULL CHECK(trigger IN ('scheduled','manual')),
    capture_method TEXT NOT NULL CHECK(capture_method IN ('desktop_duplication','none')),
    result TEXT NOT NULL CHECK(result IN ('success','missing','cleaned')),
    missing_reason TEXT CHECK(missing_reason IN ('desktop_idle','global_pause','locked','sleep','secure_desktop','session_disconnected','remote_session','privacy_exclusion','capture_failed','low_disk','retention_cleaned','user_deleted','backup_omitted','recovery_corruption')),
    timeline_started_utc_ms INTEGER NOT NULL,
    timeline_ended_utc_ms INTEGER NOT NULL,
    CHECK(timeline_ended_utc_ms>=timeline_started_utc_ms),
    CHECK((result='success' AND blob_id IS NOT NULL AND captured_at_utc_ms IS NOT NULL AND pixel_width>0 AND pixel_height>0 AND missing_reason IS NULL)
       OR (result IN ('missing','cleaned') AND blob_id IS NULL AND missing_reason IS NOT NULL)),
    UNIQUE(slot_started_utc_ms,display_key,trigger)
) STRICT;
INSERT INTO snapshot_slots_recovery SELECT * FROM snapshot_slots;
DROP TABLE snapshot_slots;
ALTER TABLE snapshot_slots_recovery RENAME TO snapshot_slots;
CREATE INDEX snapshot_slots_range ON snapshot_slots(slot_started_utc_ms,result);
CREATE INDEX snapshot_slots_blob ON snapshot_slots(blob_id) WHERE blob_id IS NOT NULL;
