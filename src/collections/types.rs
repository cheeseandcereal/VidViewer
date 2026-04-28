//! Shared types and error enum for the `collections` module.

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;

use crate::ids::{CollectionId, DirectoryId, VideoId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Directory,
    Custom,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Directory => "directory",
            Kind::Custom => "custom",
        }
    }
    pub fn from_db(s: &str) -> Option<Kind> {
        match s {
            "directory" => Some(Kind::Directory),
            "custom" => Some(Kind::Custom),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Collection {
    pub id: CollectionId,
    pub name: String,
    pub kind: Kind,
    pub directory_id: Option<DirectoryId>,
    pub hidden: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub video_count: i64,
}

/// Summary for grid/listing views.
#[derive(Debug, Clone, Serialize)]
pub struct CollectionSummary {
    #[serde(flatten)]
    pub coll: Collection,
    /// Up to 4 thumbnail video ids for a mosaic preview.
    pub preview_video_ids: Vec<VideoId>,
}

/// One directory included in a custom collection. Used for UI chip rows and
/// the detail page's directory management UI.
#[derive(Debug, Clone, Serialize)]
pub struct CollectionDirectory {
    pub directory_id: DirectoryId,
    pub label: String,
    pub path: String,
    /// `true` if this directory is currently soft-removed. Listed but not
    /// contributing to the collection until re-added.
    pub removed: bool,
    pub added_at: DateTime<Utc>,
}

/// A video card in a collection grid.
#[derive(Debug, Clone, Serialize)]
pub struct VideoCard {
    pub id: VideoId,
    pub filename: String,
    pub duration_secs: Option<f64>,
    pub thumbnail_ok: bool,
    pub preview_ok: bool,
    pub missing: bool,
    pub is_audio_only: bool,
    pub updated_at_epoch: i64,
}

#[derive(Debug, Clone, Error, Serialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum MutationError {
    #[error("collection not found")]
    NotFound,
    #[error("directory collections cannot be modified that way")]
    DirectoryCollectionImmutable,
    #[error("name must be non-empty")]
    EmptyName,
    #[error("directory not found")]
    DirectoryNotFound,
    #[error("directory is soft-removed")]
    DirectoryRemoved,
    #[error("internal error: {message}")]
    Internal { message: String },
}

impl MutationError {
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            MutationError::NotFound | MutationError::DirectoryNotFound => StatusCode::NOT_FOUND,
            MutationError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

pub(crate) fn internal<E: std::fmt::Display>(e: E) -> MutationError {
    MutationError::Internal {
        message: e.to_string(),
    }
}
