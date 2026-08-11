-- sqld resolves a request's namespace from the label *before the first dot*
-- of the `Host` header (see src/proxy.rs). A `sqld_namespace` value that is
-- allowed to contain a dot therefore lets one owner's mapping resolve to a
-- *different* owner's namespace once the gateway formats it as
-- `{namespace}.local` — a cross-tenant read/write with a perfectly valid key.
-- Whitespace/control characters are equally dangerous: they cannot be encoded
-- into an HTTP header value at all.
--
-- Constrain the column to the shape sqld itself accepts for a namespace name:
-- lowercase alphanumerics, underscores and hyphens, starting with an
-- alphanumeric, at most 63 characters.
ALTER TABLE database_mappings
    ADD CONSTRAINT sqld_namespace_format
    CHECK (sqld_namespace ~ '^[a-z0-9][a-z0-9_-]{0,62}$');
