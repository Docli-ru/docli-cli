We have decided NOT to backfill `content_changed_at` — existing rows stay NULL and fill in on their
next update, because seeding it from `updated_at` would put a wrong date on every note that was
renamed since its last edit. Put that decision into effect in `src/db/attribution.rs` (the planned
backfill goes, the field stays nullable, the comments say why), then check the result.
