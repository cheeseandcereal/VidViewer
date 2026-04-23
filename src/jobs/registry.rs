//! Runtime registry of in-flight job tasks.
//!
//! Each worker registers its per-job `AbortHandle` and the associated `video_id`
//! on claim, and deregisters on completion. Destructive directory actions consult
//! this registry to abort running jobs for the affected videos, which — together
//! with `kill_on_drop(true)` on every `tokio::process::Command` we construct —
//! terminates the underlying ffmpeg/ffprobe processes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::task::AbortHandle;

use crate::ids::VideoId;

#[derive(Debug)]
struct Entry {
    video_id: VideoId,
    handle: AbortHandle,
}

/// Shared registry of in-flight job tasks, keyed by `jobs.id`.
#[derive(Debug, Clone, Default)]
pub struct JobRegistry {
    inner: Arc<Mutex<HashMap<i64, Entry>>>,
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, job_id: i64, video_id: VideoId, handle: AbortHandle) {
        self.inner
            .lock()
            .unwrap()
            .insert(job_id, Entry { video_id, handle });
    }

    pub fn deregister(&self, job_id: i64) {
        self.inner.lock().unwrap().remove(&job_id);
    }

    /// Abort all registered jobs whose `video_id` is in `video_ids`. Returns the
    /// list of `jobs.id` values whose abort was signalled, so the caller can
    /// garbage-collect those rows from the `jobs` table.
    pub fn cancel_for_videos(&self, video_ids: &[VideoId]) -> Vec<i64> {
        let set: std::collections::HashSet<&str> = video_ids.iter().map(|v| v.as_str()).collect();
        let mut aborted = Vec::new();
        let mut guard = self.inner.lock().unwrap();
        guard.retain(|&job_id, entry| {
            if set.contains(entry.video_id.as_str()) {
                entry.handle.abort();
                aborted.push(job_id);
                false
            } else {
                true
            }
        });
        aborted
    }

    /// Count of currently-tracked running jobs. Used in tests and diagnostics.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// True when nothing is tracked.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }
}
