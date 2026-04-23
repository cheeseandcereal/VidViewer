//! Runtime registry of in-flight job tasks.
//!
//! Each worker registers its per-job `AbortHandle` and a matching
//! `CancellationToken`, along with the associated `video_id`, on claim, and
//! deregisters on completion. Destructive directory actions consult this
//! registry to cancel running jobs for the affected videos.
//!
//! The `AbortHandle` interrupts any currently-awaiting `.await` point (e.g.
//! `tokio::process::Child::wait()`), and `kill_on_drop(true)` on every ffmpeg
//! Command ensures the OS child process is SIGKILLed as the future is dropped.
//!
//! The `CancellationToken` is a cooperative flag the worker polls between
//! between synchronous operations (e.g. spawning the next ffmpeg in a preview
//! loop). This catches the window where a subtask has finished one ffmpeg and
//! is about to spawn the next — abort alone can't prevent that spawn because
//! cancellation only fires at `.await` points.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use crate::ids::VideoId;

#[derive(Debug)]
struct Entry {
    video_id: VideoId,
    handle: AbortHandle,
    token: CancellationToken,
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

    pub fn register(
        &self,
        job_id: i64,
        video_id: VideoId,
        handle: AbortHandle,
        token: CancellationToken,
    ) {
        self.inner.lock().unwrap().insert(
            job_id,
            Entry {
                video_id,
                handle,
                token,
            },
        );
    }

    pub fn deregister(&self, job_id: i64) {
        self.inner.lock().unwrap().remove(&job_id);
    }

    /// Cancel all registered jobs whose `video_id` is in `video_ids`. Returns
    /// the list of `jobs.id` values whose cancellation was signalled, so the
    /// caller can garbage-collect those rows from the `jobs` table.
    ///
    /// Both the cooperative `CancellationToken` and the hard `AbortHandle`
    /// are tripped: the token stops the next iteration of any loop that
    /// checks it, and the abort handle interrupts the currently-awaiting
    /// `.await` point (which drops the ffmpeg child and SIGKILLs it).
    pub fn cancel_for_videos(&self, video_ids: &[VideoId]) -> Vec<i64> {
        let set: std::collections::HashSet<&str> = video_ids.iter().map(|v| v.as_str()).collect();
        let mut aborted = Vec::new();
        let mut guard = self.inner.lock().unwrap();
        guard.retain(|&job_id, entry| {
            if set.contains(entry.video_id.as_str()) {
                entry.token.cancel();
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

    /// True if a job with this id is currently tracked. Used by the stuck-job
    /// watchdog to distinguish "still running in a live worker task" from
    /// "orphaned DB row with no task behind it".
    pub fn contains(&self, job_id: i64) -> bool {
        self.inner.lock().unwrap().contains_key(&job_id)
    }
}
