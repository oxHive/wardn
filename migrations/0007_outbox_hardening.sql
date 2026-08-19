-- Deferred hardening from the 2026-08-13 security audit: the outbox table
-- had no floor on `attempts` (a negative value is nonsensical but was never
-- rejected) and no index to back the background worker's
-- `WHERE status = 'pending' ORDER BY created_at` scan, which degrades to a
-- full table scan as done/failed rows accumulate.

ALTER TABLE namespace_provisioning_outbox
    ADD CONSTRAINT namespace_provisioning_outbox_attempts_non_negative
    CHECK (attempts >= 0);

CREATE INDEX idx_namespace_provisioning_outbox_pending
    ON namespace_provisioning_outbox (created_at)
    WHERE status = 'pending';
