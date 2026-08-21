---
id: team_id
name: Team Name
description: Team purpose
members:
  - preset_id: role_a
    id: role_a_primary
    name: RoleAPrimary
  - preset_id: role_a
    id: role_a_secondary
    name: RoleASecondary
---

# Team Preset Template

## Purpose
Describe when this team should be used.

## Members
- Explain why each role is included.

## Notes
- `preset_id` must match a built-in role preset id and may be reused.
- Every member instance must have a unique `id` and `name`.
- Legacy `member_ids` remains supported, but do not combine it with `members`.
