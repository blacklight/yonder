-- Epic 23 (frontend foundation): per-user alternative-frontend choice.
--
-- `users.frontend` — name of an installed frontend the user prefers.
-- NULL keeps the meaningful semantics the sibling settings
-- (`locale`/`theme`/`accent`) already use: unset = follow the
-- instance default (`admin.default_frontend`). The API layer
-- validates the value against the frontend registry, so no CHECK is
-- added here; a frontend removed later simply falls back at read
-- time.

ALTER TABLE users
    ADD COLUMN frontend TEXT;
