//! Small helpers for decoding values from a SQLite row.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{sqlite::SqliteRow, Row};

/// Decode an INTEGER column (0/1) as a Rust bool.
pub fn bool_from_i64(row: &SqliteRow, col: &str) -> bool {
    row.get::<i64, _>(col) != 0
}

/// Decode a TEXT column holding an RFC 3339 timestamp into a `DateTime<Utc>`.
pub fn datetime_from_rfc3339(row: &SqliteRow, col: &str) -> Result<DateTime<Utc>> {
    let s: String = row.get(col);
    DateTime::parse_from_rfc3339(&s)
        .with_context(|| format!("parsing {col} as RFC3339"))
        .map(|d| d.with_timezone(&Utc))
}
