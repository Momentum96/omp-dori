-- Schema v3 (additive): relay intake and replies (DESIGN 5.1, 7.3 "Relays" and "Owner"): the
-- account binding, read positions, the durable arrival queue, hand-offs, read marks, owed replies,
-- the reply outbox and open questions. Rooms and messages are text so every relay fits; `seq` and
-- `position` order messages within a room (Discord: the snowflake value).

-- The (account, owner) pair a relay's state belongs to (DISC-IN req. 3).
CREATE TABLE bindings (
    relay      TEXT PRIMARY KEY,
    account    TEXT NOT NULL,
    owner      TEXT NOT NULL,
    created_ms INTEGER NOT NULL
) STRICT;

-- The highest message fully handled per room (DISC-IN req. 29); 0 means the room was empty.
CREATE TABLE positions (
    relay      TEXT NOT NULL,
    room       TEXT NOT NULL,
    position   INTEGER NOT NULL,
    parent     TEXT,
    held       TEXT,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (relay, room)
) STRICT;

-- Durable flags, such as a catch-up read that is owed (DISC-IN req. 23).
CREATE TABLE relay_flags (
    relay TEXT NOT NULL,
    flag  TEXT NOT NULL,
    PRIMARY KEY (relay, flag)
) STRICT;

-- Owner messages admitted but not yet handled (DISC-IN req. 24-28).
CREATE TABLE arrival_queue (
    id          INTEGER PRIMARY KEY,
    relay       TEXT NOT NULL,
    room        TEXT NOT NULL,
    message     TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    parent      TEXT,
    position_at INTEGER NOT NULL,
    raw         TEXT NOT NULL,
    bytes       INTEGER NOT NULL,
    created_ms  INTEGER NOT NULL,
    UNIQUE (relay, room, message)
) STRICT;

-- One hand-off per owner message, ever (DISC-IN req. 47-52), and where its answer stands
-- (DISC-OUT req. 20-29).
CREATE TABLE handoffs (
    relay      TEXT NOT NULL,
    room       TEXT NOT NULL,
    message    TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    target     TEXT NOT NULL,
    request_id TEXT NOT NULL,
    digest     TEXT NOT NULL,
    state      TEXT NOT NULL CHECK (state IN ('handed', 'delivered', 'in_doubt', 'failed')),
    answer     TEXT NOT NULL CHECK (answer IN ('waiting', 'posting', 'posted', 'abandoned', 'dropped')),
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (relay, room, message)
) STRICT;

CREATE INDEX handoffs_open ON handoffs (relay, answer) WHERE answer IN ('waiting', 'posting');

-- Seen marks added to admitted owner messages (DISC-OUT req. 30).
CREATE TABLE read_marks (
    relay   TEXT NOT NULL,
    room    TEXT NOT NULL,
    message TEXT NOT NULL,
    at_ms   INTEGER NOT NULL,
    PRIMARY KEY (relay, room, message)
) STRICT;

-- What the owner is owed (DISC-OUT req. 32-34).
CREATE TABLE owed_replies (
    relay       TEXT NOT NULL,
    room        TEXT NOT NULL,
    message     TEXT NOT NULL,
    admitted_ms INTEGER NOT NULL,
    text        TEXT NOT NULL,
    reminders   INTEGER NOT NULL DEFAULT 0,
    settled_ms  INTEGER,
    PRIMARY KEY (relay, room, message)
) STRICT;

-- The reply outbox: parts stored before the first post; a completed source is the reply receipt
-- (DISC-OUT req. 24-29).
CREATE TABLE outbox_parts (
    relay      TEXT NOT NULL,
    room       TEXT NOT NULL,
    source     TEXT NOT NULL,
    idx        INTEGER NOT NULL,
    text       TEXT NOT NULL,
    nonce      TEXT NOT NULL,
    remote     TEXT,
    linked     INTEGER NOT NULL DEFAULT 0,
    state      TEXT NOT NULL CHECK (state IN ('pending', 'confirmed', 'dropped')),
    created_ms INTEGER NOT NULL,
    updated_ms INTEGER NOT NULL,
    PRIMARY KEY (relay, room, source, idx)
) STRICT;

-- Question and approval cards left open, closed at the next start (DISC-OUT req. 76).
CREATE TABLE open_questions (
    relay      TEXT NOT NULL,
    room       TEXT NOT NULL,
    message    TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    PRIMARY KEY (relay, room, message)
) STRICT;
