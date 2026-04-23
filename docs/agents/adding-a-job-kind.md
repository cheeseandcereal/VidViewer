# Agent playbook — Adding a job kind

Last updated: 2026-04-22

Job kinds are the discrete background work units tracked in the `jobs` table. To add one:

1. **Pick a name.** Must be a lowercase ASCII string, e.g. `waveform`. Add it to the allowed
   set in `docs/design/05-jobs-and-workers.md` and to whatever enum backs `jobs.kind` in code.
2. **Decide its lane.** General lane (concurrency 10) or preview lane (concurrency 8)? If it
   could starve other jobs, give it its own lane.
3. **Implement it.**
   - Add a module `src/jobs/<kind>.rs`.
   - Export a function like `pub async fn run(state: &AppState, video: &Video) -> anyhow::Result<()>`.
   - External-process work must go through the `VideoTool` trait, not raw `Command`.
4. **Wire into the worker loop.** `src/jobs/mod.rs` dispatches on `jobs.kind`; add a branch.
5. **Enqueue it.** Either from the scanner (for per-video assets) or from another job's success
   path (if it depends on output from a prior job).
6. **Add a migration** if the job stores state on `videos` (e.g. a `waveform_ok` column).
7. **Test it.** Write a unit test using `MockVideoTool` that asserts the job invokes the tool
   with the expected arguments.

Before committing: `just fmt && just lint && just check && just test`.
