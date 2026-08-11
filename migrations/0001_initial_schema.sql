CREATE TABLE users (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email      TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE orgs (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE org_members (
    org_id     UUID NOT NULL REFERENCES orgs(id),
    user_id    UUID NOT NULL REFERENCES users(id),
    role       TEXT NOT NULL CHECK (role IN ('admin', 'member', 'read_only')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (org_id, user_id)
);

CREATE TABLE workspaces (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_user_id  UUID NOT NULL REFERENCES users(id),
    name           TEXT NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE database_mappings (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_type     TEXT NOT NULL CHECK (owner_type IN ('user', 'workspace', 'org')),
    owner_id       UUID NOT NULL,
    sqld_namespace TEXT NOT NULL UNIQUE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner_type, owner_id)
);

CREATE TABLE api_keys (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users(id),
    owner_type  TEXT NOT NULL CHECK (owner_type IN ('user', 'workspace', 'org')),
    owner_id    UUID NOT NULL,
    prefix      TEXT NOT NULL UNIQUE,
    key_hash    TEXT NOT NULL,
    revoked_at  TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_api_keys_prefix ON api_keys (prefix) WHERE revoked_at IS NULL;
