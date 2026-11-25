-- Schema v5 (additive): lanes (LANE-13), their history, open dedupe (LANE req. 10), reports
-- (EVR req. 1-8), steps (EVR req. 30-32), evidence seen per run (EVR req. 15) and the journal of
-- lane thread operations (DISC-OUT req. 54).

-- One row per lane key. `record` is the lane's full state as JSON; the columns repeat what listings
-- filter on. `revision` counts history entries; (opened_ms, revision) is the lane's revision.
CREATE TABLE lanes (
    key        TEXT PRIMARY KEY,
    state      TEXT NOT NULL CHECK (state IN ('starting', 'running', 'held', 'claimed', 'rejected', 'closing', 'closed')),
    outcome    TEXT CHECK (outcome IN ('completed', 'parked', 'abandoned', 'failed')),
    thread     TEXT,
    opened_ms  INTEGER NOT NULL,
    closed_ms  INTEGER,
    revision   INTEGER NOT NULL,
    record     TEXT NOT NULL
) STRICT;

CREATE TABLE lane_history (
    key    TEXT NOT NULL,
    seq    INTEGER NOT NULL,
    at_ms  INTEGER NOT NULL,
    state  TEXT NOT NULL,
    note   TEXT NOT NULL,
    PRIMARY KEY (key, seq)
) STRICT;

-- Only the digest of the inputs and the outcome, never the brief.
CREATE TABLE open_dedupe (
    request_id TEXT PRIMARY KEY,
    digest     TEXT NOT NULL,
    key        TEXT NOT NULL,
    outcome    TEXT,
    created_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE reports (
    key        TEXT NOT NULL,
    number     INTEGER NOT NULL,
    run_id     TEXT NOT NULL,
    at_ms      INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    text       TEXT NOT NULL,
    summary    TEXT,
    evidence   TEXT NOT NULL,
    new_items  INTEGER NOT NULL,
    relay      TEXT NOT NULL,
    PRIMARY KEY (key, number)
) STRICT;

CREATE TABLE steps (
    key        TEXT NOT NULL,
    name       TEXT NOT NULL,
    run_id     TEXT,
    at_ms      INTEGER NOT NULL,
    result     TEXT NOT NULL,
    evidence   TEXT NOT NULL,
    receipts   TEXT NOT NULL,
    new_items  INTEGER NOT NULL,
    PRIMARY KEY (key, name)
) STRICT;

CREATE TABLE evidence_seen (
    key    TEXT NOT NULL,
    run_id TEXT NOT NULL,
    ident  TEXT NOT NULL,
    hash   TEXT NOT NULL,
    PRIMARY KEY (key, run_id, ident)
) STRICT;

CREATE TABLE thread_journal (
    source     TEXT PRIMARY KEY,
    lane       TEXT NOT NULL,
    parent     TEXT NOT NULL,
    operation  TEXT NOT NULL,
    state      TEXT NOT NULL CHECK (state IN ('pending', 'created', 'confirmed')),
    thread     TEXT,
    at_ms      INTEGER NOT NULL
) STRICT;
