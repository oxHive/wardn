CREATE TABLE namespace_provisioning_outbox (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_type     TEXT NOT NULL CHECK (owner_type IN ('user', 'org')),
    owner_id       UUID NOT NULL,
    sqld_namespace TEXT NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('pending', 'done', 'failed')) DEFAULT 'pending',
    attempts       INT NOT NULL DEFAULT 0,
    last_error     TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
