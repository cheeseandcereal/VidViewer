//! JSON API handlers.
//!
//! Handlers return `Result<Response, ApiError>`. The [`ApiError`]
//! (crate::http::error::ApiError) wraps the various module-level typed
//! errors and implements `IntoResponse`, so the error path uses `?`
//! rather than a match cascade at each site.
//!
//! Split across per-subsystem files — router entries in
//! `src/http/mod.rs` call the re-exports below by path:
//!
//! - [`directories`] — `GET|POST /api/directories`,
//!   `PATCH|DELETE /api/directories/:id`, cancel-jobs helper.
//! - [`collections`] — `GET|POST /api/collections`,
//!   `PATCH|DELETE /api/collections/:id`, videos/directories
//!   sub-resources, random.
//! - [`videos`] — `GET /api/videos/:id`, `POST /api/videos/:id/play`.
//! - [`history`] — `GET /api/history`, `DELETE /api/history/:id`.
//! - [`scan`] — `POST /api/scan`, `GET /api/scan/status`,
//!   `GET /api/directories/jobs`.
//! - [`fs`] — `GET /api/fs/list`.

pub mod collections;
pub mod directories;
pub mod fs;
pub mod history;
pub mod scan;
pub mod videos;

pub use collections::{
    add_directory_to_collection, create_collection, delete_collection, list_collection_directories,
    list_collection_videos, list_collections, random_from_collection,
    remove_directory_from_collection, rename_collection,
};
pub use directories::{add_directory, delete_directory, list_directories, patch_directory};
pub use fs::fs_list;
pub use history::{delete_history, list_history};
pub use scan::{directory_job_status, scan_status, start_scan};
pub use videos::{get_video, play_video};

#[cfg(test)]
mod test_helpers;
