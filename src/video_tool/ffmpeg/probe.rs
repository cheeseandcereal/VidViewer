//! ffprobe invocation. Shells out to `ffprobe` with JSON output and a
//! wall-clock timeout, then hands the bytes to the self-contained parser
//! in the sibling `video_tool::ffprobe` module.

use std::path::Path;
use std::process::Stdio;

use anyhow::{bail, Context, Result};

use crate::video_tool::{ffprobe::parse_ffprobe_json, ProbeResult};

pub(super) async fn probe(path: &Path) -> Result<ProbeResult> {
    // 60 seconds is *far* more than ffprobe needs for any real video —
    // typical runs are well under a second, even for huge files. A hung
    // ffprobe would otherwise leave the job row in `running` forever.
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

    let fut = tokio::process::Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-print_format")
        .arg("json")
        .arg("-show_format")
        .arg("-show_streams")
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();

    let out = match tokio::time::timeout(PROBE_TIMEOUT, fut).await {
        Ok(res) => res.with_context(|| format!("spawning ffprobe for {}", path.display()))?,
        Err(_) => bail!(
            "ffprobe timed out after {}s for {}",
            PROBE_TIMEOUT.as_secs(),
            path.display()
        ),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "ffprobe failed for {}: {} ({})",
            path.display(),
            out.status,
            stderr.trim()
        );
    }
    parse_ffprobe_json(&out.stdout)
}
