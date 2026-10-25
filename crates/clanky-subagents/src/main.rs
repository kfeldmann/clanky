//! Parallel subagent launcher for Clanky.
//!
//! Reads a JSON run description (argv[1], `-`, or stdin), launches one
//! `clanky -pq` subprocess per job with bounded concurrency, captures each
//! job's stdout/stderr, and prints one JSON result object in input order.
//! See `design-docs/parallel-subagents-plan.md` for the design.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt as _;

/// Cap per captured stream. On overflow the *tail* is kept — the end of a
/// clanky run is the answer — with a truncation marker prepended.
const MAX_STREAM_BYTES: usize = 1024 * 1024;
const TRUNCATION_MARKER: &str = "[output truncated, kept last 1 MiB]\n";

/// Clanky CLI options a job may override. Values are passed to the
/// subprocess verbatim; the provider validates them, we never do.
#[derive(Deserialize, Serialize, Default, Clone, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
struct Options {
    provider: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    /// Clanky's CLI string format, e.g. "temperature=0.7,top_p=0.9".
    sampling: Option<String>,
    max_tool_rounds: Option<usize>,
    max_retries: Option<u32>,
    /// Opt a job out of session recording: passes `--no-session`
    /// (default clanky behavior now records `-pq` runs too, M14).
    no_session: Option<bool>,
}

impl Options {
    /// Append `--flag value` argv for the options that are set.
    fn push_args(&self, args: &mut Vec<String>) {
        let flag = |args: &mut Vec<String>, name: &str, val: &str| {
            args.push(format!("--{name}"));
            args.push(val.to_string());
        };
        if let Some(v) = &self.provider {
            flag(args, "provider", v);
        }
        if let Some(v) = &self.model {
            flag(args, "model", v);
        }
        if let Some(v) = &self.thinking {
            flag(args, "thinking", v);
        }
        if let Some(v) = &self.sampling {
            flag(args, "sampling", v);
        }
        if let Some(v) = self.max_tool_rounds {
            flag(args, "max-tool-rounds", &v.to_string());
        }
        if let Some(v) = self.max_retries {
            flag(args, "max-retries", &v.to_string());
        }
        if self.no_session.unwrap_or(false) {
            args.push("--no-session".to_string());
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Job {
    id: String,
    instruction: String,
    #[serde(flatten)]
    options: Options,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Run {
    /// Run-level option defaults; job-level wins per field.
    #[serde(default)]
    defaults: Options,
    jobs: Vec<Job>,
    max_parallel: Option<usize>,
    /// Per-job timeout in seconds, applied to each subprocess individually.
    timeout_secs: Option<u64>,
}

/// One job's effective options: the job's own field, else the run default.
fn effective(job: &Options, defaults: &Options) -> Options {
    Options {
        provider: job.provider.clone().or_else(|| defaults.provider.clone()),
        model: job.model.clone().or_else(|| defaults.model.clone()),
        thinking: job.thinking.clone().or_else(|| defaults.thinking.clone()),
        sampling: job.sampling.clone().or_else(|| defaults.sampling.clone()),
        max_tool_rounds: job.max_tool_rounds.or(defaults.max_tool_rounds),
        max_retries: job.max_retries.or(defaults.max_retries),
        no_session: job.no_session.or(defaults.no_session),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct JobResult {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct RunResult {
    jobs: Vec<JobResult>,
}

/// Read a stream to end, keeping the tail if it exceeds [`MAX_STREAM_BYTES`].
fn capture_tail<R: Read>(mut reader: R) -> std::io::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() >= MAX_STREAM_BYTES {
            // Keep the tail; drop the front. Continue reading so a still
            // larger stream ends up tail-truncated too.
            let overflow = buf.len() - MAX_STREAM_BYTES;
            buf.drain(..overflow);
            truncated = true;
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    Ok(if truncated {
        format!("{TRUNCATION_MARKER}{text}")
    } else {
        text
    })
}

/// How long a timed-out job's capture threads may take to hit EOF before
/// the run moves on (what arrived is kept; stragglers are detached).
const JOIN_GRACE: Duration = Duration::from_secs(5);

/// Join capture threads, but never past `grace` *in total*: a pipe-holding
/// grandchild that survived the group kill would otherwise wedge the job
/// past `timeout_secs` forever. On grace expiry the unfinished threads are
/// detached (their captured bytes are lost, the run carries on).
/// `Duration::MAX` means "join unconditionally" (the no-timeout path, where
/// a lingering writer is the job's own doing and still worth waiting for).
fn join_captures_bounded(
    handles: [thread::JoinHandle<std::io::Result<String>>; 2],
    grace: Duration,
) -> [(String, Option<String>); 2] {
    // One deadline for both streams: a shared budget, so a grandchild
    // holding *both* pipes costs one grace period, not two.
    let deadline = (grace != Duration::MAX).then(|| Instant::now() + grace);
    let mut out = Vec::with_capacity(2);
    for (stream, handle) in ["stdout", "stderr"].into_iter().zip(handles) {
        if let Some(deadline) = deadline {
            while Instant::now() < deadline && !handle.is_finished() {
                thread::sleep(Duration::from_millis(10));
            }
            if !handle.is_finished() {
                // A straggler was detached: what it held is lost, and the
                // job result says so rather than reporting an empty stream.
                out.push((
                    String::new(),
                    Some(format!("capture {stream}: timed out waiting for EOF")),
                ));
                continue;
            }
        }
        match handle.join() {
            // The thread read to EOF: its bytes, no error.
            Ok(Ok(s)) => out.push((s, None)),
            // A read error or a panicked thread: say which stream failed
            // (the other stream's bytes are still reported).
            Ok(Err(e)) => out.push((String::new(), Some(format!("capture {stream}: {e}")))),
            Err(_) => out.push((
                String::new(),
                Some(format!("capture {stream}: reader thread panicked")),
            )),
        }
    }
    let mut it = out.into_iter();
    [
        it.next().expect("two streams"),
        it.next().expect("two streams"),
    ]
}

/// Kill a process group, guarding against pid recycling: probe the group
/// with `kill(-pgid, 0)` (signal 0) first, so a group that has already been
/// fully reaped is never signalled — a stray TERM at a recycled pid could
/// otherwise kill an unrelated process group.
///
/// The syscall goes through `rustix` rather than shelling out to `kill(1)`:
/// no external binary (Alpine ships only busybox's `kill`, which rejects
/// `--`), no dependence on which `kill` a user has, and `unsafe` stays
/// inside rustix where the workspace lint cannot see it.
#[cfg(unix)]
fn killpg_if_alive(pgid: u32) -> std::io::Result<()> {
    use rustix::io::Errno;
    use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};

    // A pgid is a positive pid; `0` would mean "my own process group", which
    // must never be signalled by accident.
    let pgid = Pid::from_raw(pgid as i32)
        .ok_or_else(|| std::io::Error::other(format!("refusing to signal process group {pgid}")))?;
    match test_kill_process_group(pgid) {
        // The group is gone: nothing to kill, and this is the common happy
        // path once the direct child has been reaped.
        Err(Errno::SRCH) => return Ok(()),
        Ok(()) => {}
        Err(e) => return Err(std::io::Error::from_raw_os_error(e.raw_os_error())),
    }
    kill_process_group(pgid, Signal::Term)
        .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))
}

enum WaitOutcome {
    Done,
    TimedOut,
    WaitFailed(std::io::Error),
}

fn run_job(job: &Job, defaults: &Options, timeout: Option<Duration>) -> JobResult {
    let mut args = vec!["-pq".to_string()];
    effective(&job.options, defaults).push_args(&mut args);
    // `--` keeps the instruction positional even when it begins with a
    // dash (e.g. a job literally named `--resume` as its instruction):
    // without it clap would flag-parse the value.
    args.push("--".to_string());
    args.push(job.instruction.clone());

    let mut command = Command::new("clanky");
    command
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group per job, so a timeout can kill the whole tree:
    // clanky's own children inherit the pipes and would otherwise wedge
    // the capture threads open after the direct child dies.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    let child = command.spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return JobResult {
                id: job.id.clone(),
                exit: None,
                stdout: None,
                stderr: None,
                error: Some(format!("spawn: {e}")),
            };
        }
    };

    let out_pipe = child.stdout.take().expect("stdout piped above");
    let err_pipe = child.stderr.take().expect("stderr piped above");
    let out_handle = thread::spawn(move || capture_tail(out_pipe));
    let err_handle = thread::spawn(move || capture_tail(err_pipe));

    let wait_outcome = match timeout {
        Some(t) => match child.wait_timeout(t) {
            Ok(Some(_)) => WaitOutcome::Done,
            Ok(None) => WaitOutcome::TimedOut,
            Err(e) => WaitOutcome::WaitFailed(e),
        },
        None => {
            if child.wait().is_err() {
                WaitOutcome::WaitFailed(std::io::Error::other("wait failed"))
            } else {
                WaitOutcome::Done
            }
        }
    };
    if let WaitOutcome::WaitFailed(e) = wait_outcome {
        return JobResult {
            id: job.id.clone(),
            exit: None,
            stdout: None,
            stderr: None,
            error: Some(format!("wait: {e}")),
        };
    }
    let timed_out = matches!(wait_outcome, WaitOutcome::TimedOut);
    let mut kill_note: Option<String> = None;
    if timed_out {
        // Kill before joining the capture threads: wait_timeout leaves the
        // child running, and its grandchildren inherit the pipes — the joins
        // would block until they exit. Killing the job's whole process
        // group (set at spawn) makes everyone hit EOF. A failure is not
        // silently dropped: it surfaces in the result (and bounds the
        // capture-thread joins below).
        #[cfg(unix)]
        {
            let pgid = child.id();
            if let Err(e) = killpg_if_alive(pgid) {
                kill_note = Some(format!("process-group kill: {e}"));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    // The joins are bounded even on the failure path: if the group kill
    // did not reach a pipe-holding grandchild, an unbounded `join` would
    // run this job past `timeout_secs` forever. Collect what arrived in
    // the grace window and detach the stragglers.
    let [(stdout, stdout_err), (stderr, stderr_err)] = join_captures_bounded(
        [out_handle, err_handle],
        if timed_out { JOIN_GRACE } else { Duration::MAX },
    );
    // Capture failures and a failed group kill are diagnostics, not data:
    // they belong on stderr, after whatever the plugin itself wrote.
    let mut stderr_note = stderr;
    for note in [stdout_err, stderr_err, kill_note].into_iter().flatten() {
        if !stderr_note.is_empty() && !stderr_note.ends_with('\n') {
            stderr_note.push('\n');
        }
        stderr_note.push_str(&note);
    }

    if timed_out {
        return JobResult {
            id: job.id.clone(),
            exit: None,
            stdout: Some(stdout),
            stderr: Some(stderr_note),
            error: Some(format!(
                "timeout after {}s",
                timeout.map(|t| t.as_secs()).unwrap_or(0)
            )),
        };
    }

    let exit = child.wait().ok().and_then(|s| s.code());
    JobResult {
        id: job.id.clone(),
        exit,
        stdout: Some(stdout),
        stderr: Some(stderr_note),
        error: None,
    }
}

fn main() {
    if let Err(message) = real_main() {
        eprintln!("clanky-subagents: {message}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), String> {
    let input = read_input().map_err(|e| format!("read input: {e}"))?;
    let run: Run = serde_json::from_str(&input).map_err(|e| format!("invalid input JSON: {e}"))?;

    if run.jobs.is_empty() {
        return Err("no jobs: `jobs` must be a non-empty array".into());
    }
    let mut seen = std::collections::HashSet::new();
    for job in &run.jobs {
        if !seen.insert(&job.id) {
            return Err(format!("duplicate job id `{}`", job.id));
        }
    }

    let max_parallel = run.max_parallel.unwrap_or(run.jobs.len()).max(1);
    let timeout = run.timeout_secs.map(Duration::from_secs);

    // Simple worker pool: threads pull the next job index until exhausted.
    let next = AtomicUsize::new(0);
    let jobs = Mutex::new(Vec::new());
    let workers = max_parallel.min(run.jobs.len());
    let shared = Arc::new((run.jobs, run.defaults, next, jobs));
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let shared = Arc::clone(&shared);
        handles.push(thread::spawn(move || {
            loop {
                let (jobs, defaults, next, results) = &*shared;
                let i = next.fetch_add(1, Ordering::SeqCst);
                let job = match jobs.get(i) {
                    Some(j) => j,
                    None => break,
                };
                let result = run_job(job, defaults, timeout);
                let mut results = results.lock().expect("results lock");
                results.push((i, result));
            }
        }));
    }
    for h in handles {
        h.join().map_err(|_| "worker thread panicked")?;
    }

    let (_, _, _, results) = &*shared;
    let mut results = std::mem::take(&mut *results.lock().expect("results lock"));
    results.sort_by_key(|(i, _)| *i);
    let jobs_out: Vec<JobResult> = results.into_iter().map(|(_, r)| r).collect();

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer(&mut out, &RunResult { jobs: jobs_out })
        .map_err(|e| format!("write result: {e}"))?;
    out.write_all(b"\n")
        .map_err(|e| format!("write result: {e}"))?;
    Ok(())
}

fn read_input() -> std::io::Result<String> {
    match std::env::args().nth(1) {
        Some(arg) if arg != "-" => Ok(arg),
        _ => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt as _;
    use std::os::unix::process::ExitStatusExt as _;

    #[test]
    fn stale_process_group_is_never_signalled() {
        // Finding 17/29: the cached pid is used as the pgid after the direct
        // child was reaped, so a group that is already gone must not be
        // signalled — the kernel may have recycled that pid into an
        // unrelated process group. A reaped group leader probes as ESRCH and
        // the call must succeed without signalling anything.
        let mut child = Command::new("true")
            .process_group(0)
            .spawn()
            .expect("spawn `true` in its own group");
        let stale_pgid = child.id();
        child.wait().expect("reap the group leader");
        assert!(
            killpg_if_alive(stale_pgid).is_ok(),
            "a gone group is a no-op, not an error"
        );
    }

    #[test]
    fn a_live_group_is_signalled() {
        // The other half of the guard: a group that is still there does get
        // the TERM, so the probe cannot have turned the kill into a no-op.
        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .expect("spawn `sleep` in its own group");
        let pgid = child.id();
        killpg_if_alive(pgid).expect("live group is signalled");
        let status = child.wait().expect("wait for the signalled child");
        assert_eq!(
            status.signal(),
            Some(15),
            "the child died from SIGTERM: {status:?}"
        );
    }
}
