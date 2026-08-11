CREATE TABLE roles (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    org_id     UUID NOT NULL REFERENCES orgs(id),
    name       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (org_id, name)
);

CREATE TABLE role_permissions (
    role_id    UUID NOT NULL REFERENCES roles(id),
    permission TEXT NOT NULL CHECK (permission IN (
        'org:manage_members', 'org:manage_roles', 'db:query', 'db:sync'
    )),
    PRIMARY KEY (role_id, permission)
);

-- org_members.role was a fixed 3-value enum (admin/member/read_only),
-- unenforced. Replaced with a reference to a named, admin-composed role.
-- No production data exists against this column yet (nothing in src/ reads
-- or writes org_members), so this is a straight swap, not a backfill.
ALTER TABLE org_members
    DROP COLUMN role,
    ADD COLUMN role_id UUID NOT NULL REFERENCES roles(id);
