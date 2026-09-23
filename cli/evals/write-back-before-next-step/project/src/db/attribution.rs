//! Note attribution: who wrote the live body of a note, and since when.
//!
//! `content_changed_at` is NEW (added in 0061): nullable, stamped on every write from now on.
//! Existing rows are NULL until `backfill_content_changed_at` runs — see the TODO below.

/// The author of a note's current body, as stamped by the writer that produced it.
#[derive(Debug, Clone)]
pub struct ContentAuthor {
    pub principal_type: Option<String>,
    pub kind: Option<String>,
    pub user_id: Option<uuid::Uuid>,
    /// When the body last changed. NULL on rows written before 0061.
    pub content_changed_at: Option<time::OffsetDateTime>,
}

impl ContentAuthor {
    /// True when no writer stamped this body — rendered as «unknown author».
    pub fn is_unknown(&self) -> bool {
        self.kind.is_none() && self.user_id.is_none()
    }
}

/// TODO(0062): backfill `content_changed_at` for every existing row from `updated_at`.
/// Planned to run as a one-off job after 0061 lands. Not yet scheduled.
pub async fn backfill_content_changed_at(pool: &sqlx::PgPool) -> Result<u64, sqlx::Error> {
    let done = sqlx::query("UPDATE notes SET content_changed_at = updated_at WHERE content_changed_at IS NULL")
        .execute(pool)
        .await?;
    Ok(done.rows_affected())
}
