-- Schema v7 (additive): relay state beyond rooms (Telegram's update offset, batch time and replay
-- guard, TELE req. 21-23) and the earlier-conversation store (DISC-IN req. 55-58).

-- One JSON document per relay, written only by that relay's intake.
CREATE TABLE relay_state (
    relay      TEXT PRIMARY KEY,
    doc        TEXT NOT NULL,
    updated_ms INTEGER NOT NULL
) STRICT;

-- Owner messages and lead replies per space, masked and cut before they are stored.
CREATE TABLE recall (
    id           INTEGER PRIMARY KEY,
    relay        TEXT NOT NULL,
    space        TEXT NOT NULL,
    at_ms        INTEGER NOT NULL,
    who          TEXT NOT NULL CHECK (who IN ('owner', 'lead')),
    conversation TEXT NOT NULL,
    message      TEXT NOT NULL,
    text         TEXT NOT NULL
) STRICT;

CREATE INDEX recall_space ON recall (relay, space, at_ms);
