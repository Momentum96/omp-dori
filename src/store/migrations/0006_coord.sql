-- Schema v6 (additive): coordination records (milestone 3b). Each record is a JSON document of one
-- kind: queued launches (LANE-11), work items (COORD-25), planner runs and the planner's own state
-- (COORD-22, COORD-23), review verdicts (COORD-38), published files (EVR-17), learned rules
-- (EVR-27) and board posts (EVR-30). `version` counts writes so a caller can compare and swap.
CREATE TABLE coord (
    kind     TEXT NOT NULL,
    id       TEXT NOT NULL,
    version  INTEGER NOT NULL,
    at_ms    INTEGER NOT NULL,
    doc      TEXT NOT NULL,
    PRIMARY KEY (kind, id)
) STRICT;
