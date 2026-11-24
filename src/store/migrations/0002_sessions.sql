-- Schema v2 (additive): hosted sessions, the per-target inbox, deliveries (PATCH-01), receipts
-- (HOST-16) and once-only identities (COORD-12).

-- One row per started OMP session, written before the child is spawned (HOST req. 6).
CREATE TABLE sessions (
    handle        TEXT PRIMARY KEY,
    role          TEXT NOT NULL CHECK (role IN ('lead', 'duty', 'lane')),
    runtime       TEXT NOT NULL CHECK (runtime IN ('headless', 'tern')),
    cwd           TEXT NOT NULL,
    name          TEXT NOT NULL,
    model         TEXT NOT NULL,
    thinking      TEXT,
    mode          TEXT NOT NULL CHECK (mode IN ('fresh', 'resume')),
    expected_id   TEXT,
    expected_file TEXT,
    state         TEXT NOT NULL CHECK (state IN ('starting', 'live', 'stopped', 'failed', 'orphan_held', 'lost', 'held')),
    session_id    TEXT,
    session_file  TEXT,
    pid           INTEGER,
    started_ms    INTEGER NOT NULL,
    updated_ms    INTEGER NOT NULL,
    stop_how      TEXT,
    stop_detail   TEXT,
    stop_ms       INTEGER,
    fault         TEXT
) STRICT;

CREATE INDEX sessions_role ON sessions (role, started_ms);

-- Messages waiting for a logical target (the lead, a lane key); the mailbox outlives sessions.
CREATE TABLE inbox (
    id          INTEGER PRIMARY KEY,
    target      TEXT NOT NULL,
    sender      TEXT NOT NULL,
    text        TEXT NOT NULL,
    urgent      INTEGER NOT NULL,
    owner       INTEGER NOT NULL,
    identity    TEXT,
    state       TEXT NOT NULL CHECK (state IN ('pending', 'sent', 'delivered', 'in_doubt', 'failed')),
    delivery_id TEXT,
    created_ms  INTEGER NOT NULL,
    updated_ms  INTEGER NOT NULL
) STRICT;

CREATE INDEX inbox_open ON inbox (target, state) WHERE state IN ('pending', 'sent', 'in_doubt');

-- One batch written into one session (PATCH-01 rule 1); its text carries the delivery ID marker.
CREATE TABLE deliveries (
    id           TEXT PRIMARY KEY,
    target       TEXT NOT NULL,
    handle       TEXT NOT NULL,
    session_id   TEXT NOT NULL,
    session_file TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('wake', 'follow_up', 'steer')),
    text         TEXT NOT NULL,
    state        TEXT NOT NULL CHECK (state IN ('sent', 'delivered', 'in_doubt', 'failed', 'misdelivered', 'superseded')),
    resent       INTEGER NOT NULL DEFAULT 0,
    created_ms   INTEGER NOT NULL,
    sent_ms      INTEGER NOT NULL,
    confirmed_ms INTEGER,
    detail       TEXT
) STRICT;

CREATE INDEX deliveries_open ON deliveries (target, state) WHERE state IN ('sent', 'in_doubt');

-- Caller request IDs per target with the digest of kind and text (HOST req. 56-61).
CREATE TABLE receipts (
    target     TEXT NOT NULL,
    request_id TEXT NOT NULL,
    digest     TEXT NOT NULL,
    inbox_id   INTEGER,
    created_ms INTEGER NOT NULL,
    PRIMARY KEY (target, request_id)
) STRICT;

-- Stable identities delivered in the last 30 days (COORD req. 19).
CREATE TABLE once_ids (
    target   TEXT NOT NULL,
    identity TEXT NOT NULL,
    at_ms    INTEGER NOT NULL,
    PRIMARY KEY (target, identity)
) STRICT;
