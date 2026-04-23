//! Typed identifier newtypes.
//!
//! Use these everywhere instead of bare `String`/`i64` IDs so the type system prevents
//! passing (say) a collection id to a function that wants a video id. See AGENTS.md.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A video id. Stored as a UUID string in SQLite. Stable across file content changes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VideoId(pub String);

impl VideoId {
    pub fn new_random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VideoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for VideoId {
    fn from(s: String) -> Self {
        VideoId(s)
    }
}

/// A collection id. Auto-incrementing INTEGER primary key in SQLite.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CollectionId(pub i64);

impl CollectionId {
    pub fn raw(self) -> i64 {
        self.0
    }
}

impl fmt::Display for CollectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A directory id. Auto-incrementing INTEGER primary key in SQLite.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DirectoryId(pub i64);

impl DirectoryId {
    pub fn raw(self) -> i64 {
        self.0
    }
}

impl fmt::Display for DirectoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
