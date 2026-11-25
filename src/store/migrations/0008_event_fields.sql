-- Schema v8 (additive): an event's named fields as one JSON object, so a destination can word the
-- event itself (the ops panel writes it in the owner's language).

ALTER TABLE events ADD COLUMN fields TEXT;
