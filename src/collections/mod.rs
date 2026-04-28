//! Collections: directory-backed and custom.
//!
//! See `docs/design/07-collections.md` for the behavioral spec.
//!
//! Membership is computed on read; there is no materialized membership table.
//! Directory collections read their videos via `videos.directory_id =
//! collections.directory_id`. Custom collections read their videos as the union
//! of videos whose `directory_id` appears in the `collection_directories` rows
//! for that collection. Videos with `missing = 1` are excluded everywhere.
//!
//! The module is split across sibling files:
//! - [`types`] — `Kind`, `Collection`, `CollectionSummary`,
//!   `CollectionDirectory`, `VideoCard`, and the `MutationError` enum.
//! - [`reads`] — `list`, `get`, `list_summaries`, `videos_in`,
//!   `random_video`, `directories_in`. All read queries.
//! - [`mutations`] — `create_custom`, `rename`, `delete_custom`,
//!   `add_directory`, `remove_directory`. All writes.

pub mod mutations;
pub mod reads;
mod types;

#[cfg(test)]
mod test_helpers;

pub use mutations::{add_directory, create_custom, delete_custom, remove_directory, rename};
pub use reads::{directories_in, get, list, list_summaries, random_video, videos_in};
pub use types::{
    Collection, CollectionDirectory, CollectionSummary, Kind, MutationError, VideoCard,
};
