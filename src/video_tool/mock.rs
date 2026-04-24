//! In-memory mock `VideoTool` used in tests. Records invocations and returns
//! preconfigured `probe` results; the `thumbnail` and `previews` methods write
//! a fake file to the destination path so callers that stat it succeed.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::video_tool::{PreviewPlan, ProbeResult, VideoTool};

#[derive(Debug, Default, Clone)]
pub struct MockVideoTool {
    inner: Arc<Mutex<MockState>>,
}

#[derive(Debug, Default)]
struct MockState {
    pub probe_results: std::collections::HashMap<PathBuf, ProbeResult>,
    pub calls: Vec<MockCall>,
}

#[derive(Debug, Clone)]
pub enum MockCall {
    Probe(PathBuf),
    Thumbnail {
        src: PathBuf,
        dst: PathBuf,
        at_secs: f64,
        width: u32,
        stream_index: Option<i64>,
    },
    Preview {
        src: PathBuf,
        dst: PathBuf,
        plan: PreviewPlan,
        duration_secs: f64,
    },
}

impl MockVideoTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_probe(&self, path: PathBuf, res: ProbeResult) {
        self.inner.lock().unwrap().probe_results.insert(path, res);
    }

    pub fn calls(&self) -> Vec<MockCall> {
        self.inner.lock().unwrap().calls.clone()
    }
}

#[async_trait]
impl VideoTool for MockVideoTool {
    async fn probe(&self, path: &Path) -> Result<ProbeResult> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(MockCall::Probe(path.to_path_buf()));
        st.probe_results
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow!("no mock probe result for {}", path.display()))
    }

    async fn thumbnail(
        &self,
        src: &Path,
        dst: &Path,
        at_secs: f64,
        width: u32,
        stream_index: Option<i64>,
    ) -> Result<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(MockCall::Thumbnail {
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
            at_secs,
            width,
            stream_index,
        });
        // Pretend to write the file so callers that stat it succeed.
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(dst, b"fake");
        Ok(())
    }

    async fn previews(
        &self,
        src: &Path,
        dst: &Path,
        plan: &PreviewPlan,
        duration_secs: f64,
        _cancel: &CancellationToken,
    ) -> Result<()> {
        let mut st = self.inner.lock().unwrap();
        st.calls.push(MockCall::Preview {
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
            plan: plan.clone(),
            duration_secs,
        });
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(dst, b"fake");
        Ok(())
    }
}
