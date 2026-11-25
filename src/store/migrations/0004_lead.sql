-- Schema v4 (additive): the lead's records (DESIGN 7.3 "Owner"; EVR-22 to EVR-25; OJ-22; WS-02,
-- WS-04): the request log, the owner question ledger, messages from other people left
-- unanswered, and the lead's stream jobs. `meta` holds `owner_last_ms` (the owner's latest
-- admitted message, OJ req. 50) and `lead_cursor` (the last event routed to the lead).

-- One entry per owner request, opened by the hub on admission (DESIGN 12.2 C16).
CREATE TABLE requests (
    id         INTEGER PRIMARY KEY,
    title      TEXT NOT NULL,
    source     TEXT NOT NULL UNIQUE,
    thread     TEXT,
    next       TEXT,
    notes      TEXT NOT NULL DEFAULT '',
    result     TEXT,
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    closed_ms  INTEGER
) STRICT;

CREATE INDEX requests_open ON requests (id) WHERE closed_ms IS NULL;

-- Questions the lead put to the owner (EVR req. 34-43). `options` is a JSON array of strings.
CREATE TABLE questions (
    id          TEXT PRIMARY KEY,
    text        TEXT NOT NULL,
    source      TEXT NOT NULL,
    thread      TEXT,
    options     TEXT NOT NULL,
    recommended INTEGER,
    lead_may    INTEGER NOT NULL DEFAULT 0,
    request     INTEGER,
    room        TEXT,
    message     TEXT,
    state       TEXT NOT NULL CHECK (state IN ('open', 'answered', 'chosen', 'withdrawn')),
    outcome     TEXT,
    created_ms  INTEGER NOT NULL,
    settled_ms  INTEGER
) STRICT;

-- One open question per posted message (EVR req. 36).
CREATE UNIQUE INDEX questions_bound ON questions (room, message)
    WHERE state = 'open' AND message IS NOT NULL;

-- Messages from other people in watched conversation rooms (OJ req. 53-54).
CREATE TABLE unanswered (
    relay    TEXT NOT NULL,
    room     TEXT NOT NULL,
    message  TEXT NOT NULL,
    author   TEXT NOT NULL,
    text     TEXT NOT NULL,
    at_ms    INTEGER NOT NULL,
    reported INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (relay, room, message)
) STRICT;

-- The lead's stream jobs, restarted with the hub (WS req. 71-75).
CREATE TABLE stream_jobs (
    name       TEXT PRIMARY KEY,
    spec       TEXT NOT NULL,
    created_ms INTEGER NOT NULL
) STRICT;
