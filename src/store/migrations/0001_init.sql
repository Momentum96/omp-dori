-- Schema v1: meta, the event stream with its deliveries, jobs, job runs and transition memory.

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE events (
    id        INTEGER PRIMARY KEY,
    at_ms     INTEGER NOT NULL,
    job       TEXT NOT NULL,
    subject   TEXT NOT NULL,
    kind      TEXT NOT NULL,
    severity  TEXT NOT NULL CHECK (severity IN ('info', 'attention', 'warning', 'error')),
    text      TEXT NOT NULL,
    body      TEXT,
    body_path TEXT,
    lane      TEXT,
    thread    TEXT,
    urgent    INTEGER NOT NULL,
    dry       INTEGER NOT NULL
) STRICT;

CREATE INDEX events_at ON events (at_ms);

-- One row per (event, destination): the receipt of that outside effect.
CREATE TABLE event_deliveries (
    event_id    INTEGER NOT NULL REFERENCES events (id) ON DELETE CASCADE,
    destination TEXT NOT NULL,
    state       TEXT NOT NULL CHECK (state IN ('waiting', 'attempting', 'sent', 'failed', 'uncertain')),
    detail      TEXT,
    updated_ms  INTEGER NOT NULL,
    PRIMARY KEY (event_id, destination)
) STRICT;

CREATE INDEX event_deliveries_open ON event_deliveries (state) WHERE state IN ('waiting', 'attempting');

CREATE TABLE jobs (
    name        TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    trigger     TEXT NOT NULL,
    held        INTEGER NOT NULL DEFAULT 0,
    consumed    INTEGER NOT NULL DEFAULT 0,
    needs_ack   INTEGER NOT NULL DEFAULT 0,
    next_due_ms INTEGER,
    skipped     INTEGER NOT NULL DEFAULT 0,
    created_ms  INTEGER NOT NULL
) STRICT;

CREATE TABLE job_runs (
    id           INTEGER PRIMARY KEY,
    job          TEXT NOT NULL,
    dry          INTEGER NOT NULL,
    scheduled_ms INTEGER NOT NULL,
    started_ms   INTEGER NOT NULL,
    pid          INTEGER,
    ended_ms     INTEGER,
    exit_code    INTEGER,
    signal       INTEGER,
    timed_out    INTEGER NOT NULL DEFAULT 0,
    state        TEXT NOT NULL CHECK (state IN ('running', 'finished', 'applied', 'broken')),
    outcome      TEXT,
    stdout_path  TEXT,
    stderr_path  TEXT,
    bytes        INTEGER,
    output_hash  TEXT,
    kept         INTEGER NOT NULL DEFAULT 1
) STRICT;

CREATE INDEX job_runs_job ON job_runs (job, id);
CREATE INDEX job_runs_open ON job_runs (state) WHERE state IN ('running', 'finished');

-- Transition memory per (job, subject, condition); dry-run state is kept apart.
CREATE TABLE job_memory (
    job        TEXT NOT NULL,
    subject    TEXT NOT NULL,
    condition  TEXT NOT NULL,
    dry        INTEGER NOT NULL,
    value      TEXT NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (job, subject, condition, dry)
) STRICT;
