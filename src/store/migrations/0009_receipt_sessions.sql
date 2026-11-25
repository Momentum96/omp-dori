-- Schema v9 (additive): a receipt belongs to the session its message was written into (HOST req.
-- 57-60); NULL until then. The cap of settled receipts counts per session.

ALTER TABLE receipts ADD COLUMN session TEXT;

CREATE INDEX receipts_session ON receipts (target, session, created_ms);
CREATE INDEX receipts_inbox ON receipts (inbox_id);

-- The receipts the relays still read again (hand-offs not yet settled) are never evicted.
CREATE INDEX handoffs_handed ON handoffs (target, request_id) WHERE state = 'handed';
