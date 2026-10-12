CREATE TABLE snapshot_containers (
    local_day TEXT PRIMARY KEY CHECK (local_day GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'),
    file_name TEXT NOT NULL UNIQUE,
    committed_bytes INTEGER NOT NULL CHECK (committed_bytes >= 8)
) STRICT;

CREATE TABLE snapshot_container_records (
    local_day TEXT NOT NULL REFERENCES snapshot_containers(local_day) ON DELETE CASCADE,
    blob_id INTEGER NOT NULL REFERENCES snapshot_blobs(blob_id) ON DELETE CASCADE,
    record_offset INTEGER NOT NULL CHECK (record_offset >= 8),
    record_bytes INTEGER NOT NULL CHECK (record_bytes > 36),
    record_key BLOB NOT NULL CHECK (length(record_key) = 32),
    webp_sha256 BLOB NOT NULL CHECK (length(webp_sha256) = 32),
    PRIMARY KEY (local_day, blob_id),
    UNIQUE (local_day, record_offset)
) STRICT;

CREATE TABLE snapshot_slot_days (
    slot_id INTEGER PRIMARY KEY REFERENCES snapshot_slots(slot_id) ON DELETE CASCADE,
    local_day TEXT NOT NULL CHECK (local_day GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]')
) STRICT;

CREATE INDEX snapshot_slot_days_day ON snapshot_slot_days(local_day, slot_id);
