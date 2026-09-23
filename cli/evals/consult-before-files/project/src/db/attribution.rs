//! Note attribution: who wrote the live body of a note.
//!
//! A note whose `body_author_kind` is NULL renders as «unknown author» in the history panel.
//! See `ContentAuthor` and the `UNKNOWN` sentinel below.

/// The author of a note's current body, as stamped by the writer that produced it.
#[derive(Debug, Clone)]
pub struct ContentAuthor {
    pub principal_type: Option<String>,
    pub kind: Option<String>,
    pub user_id: Option<uuid::Uuid>,
}

impl ContentAuthor {
    /// True when no writer stamped this body — rendered as «unknown author».
    pub fn is_unknown(&self) -> bool {
        self.kind.is_none() && self.user_id.is_none()
    }
}
