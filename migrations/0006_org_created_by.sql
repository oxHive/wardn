-- The org-creation quota (POST /orgs, src/registration.rs) must count orgs
-- this user actually *created*, not orgs where they happen to hold a role
-- named 'owner' — role names are assignable by any org:manage_members
-- holder via add_member/assign_member_role, so keying the quota on role
-- name let an attacker permanently exhaust a victim's quota by adding them
-- to attacker-created orgs under an 'owner'-named role. created_by is set
-- once, by the application, at the moment of creation, and is never
-- mutable via any admin endpoint.
ALTER TABLE orgs ADD COLUMN created_by UUID REFERENCES users(id);
