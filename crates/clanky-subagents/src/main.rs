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

/// How long a job's capture threads may wait for EOF after the job's process
/// exited before the run moves on. Applies to *every* exit path, not just the
/// timeout path: the direct child exiting does not close the pipes — a
/// backgrounded helper it left behind still holds them — so an unbounded join
/// would wedge the whole batch on a job that has already finished (or been
/// killed).
const JOIN_GRACE: Duration = Duration::from_secs(5);

/// How long the readers may take to observe EOF after a pipe-holding group
/// was sent TERM, before it is escalated to KILL.
#[cfg(unix)]
const TERM_FLUSH: Duration = Duration::from_millis(500);

/// How long the readers may take to observe EOF once the group was sent
/// KILL, before the run moves on and their bytes are lost.
#[cfg(unix)]
const KILL_FLUSH: Duration = Duration::from_secs(1);

/// Join the capture threads, but never past `grace` *in total*: a writer that
/// outlived the job — a backgrounded or `setsid`'d helper holding a pipe —
/// would otherwise wedge the job forever, timeout or no timeout.
///
/// On grace expiry the group is signalled (TERM, then KILL, the same
/// escalation the bash tool uses) and the readers get a short
/// [`KILL_FLUSH`] to hit EOF, so what they captured lands in the result
/// instead of being discarded. Only a writer that escaped the group entirely
/// is left detached, and then the result says so rather than reporting an
/// empty stream.
fn join_captures_bounded(
    handles: [thread::JoinHandle<std::io::Result<String>>; 2],
    pgid: u32,
    grace: Duration,
) -> [(String, Option<String>); 2] {
    // One deadline for both streams: a shared budget, so a helper holding
    // *both* pipes costs one grace period, not two.
    let deadline = Instant::now() + grace;
    let mut pending: Vec<(usize, thread::JoinHandle<std::io::Result<String>>)> =
        handles.into_iter().enumerate().collect();
    let mut joined: [Option<std::io::Result<String>>; 2] = [None, None];
    wait_for_readers(&mut pending, &mut joined, deadline);
    if !pending.is_empty() {
        // Last resort: the job is gone but something it spawned still holds
        // the pipes, so close them for it — TERM first, then KILL, so a
        // helper that still has output to flush gets to write it. The probe
        // inside `killpg_if_alive` keeps a fully-reaped group from being
        // signalled; a writer that escaped the group entirely (its own
        // `setsid`) survives, and its stream is reported as lost below
        // rather than as empty.
        #[cfg(unix)]
        {
            use rustix::process::Signal;
            let _ = killpg_if_alive(pgid, Signal::Term);
            wait_for_readers(&mut pending, &mut joined, Instant::now() + TERM_FLUSH);
            if !pending.is_empty() {
                let _ = killpg_if_alive(pgid, Signal::Kill);
            }
        }
        wait_for_readers(&mut pending, &mut joined, Instant::now() + KILL_FLUSH);
    }
    std::array::from_fn(|i| match joined[i].take() {
        // The thread read to EOF: its bytes, no error.
        Some(Ok(s)) => (s, None),
        // A read error or a panicked thread: say which stream failed (the
        // other stream's bytes are still reported).
        Some(Err(e)) => (String::new(), Some(format!("capture {}: {e}", STREAMS[i]))),
        // Still blocked at the end of the grace: the reader is detached and
        // what it held is lost, which the result says rather than showing
        // the stream as legitimately empty.
        None => (
            String::new(),
            Some(format!(
                "capture {}: timed out waiting for EOF (a child of the job is still holding it)",
                STREAMS[i]
            )),
        ),
    })
}

/// The stream names, in the order [`join_captures_bounded`] takes them.
const STREAMS: [&str; 2] = ["stdout", "stderr"];

/// Join every reader that finished before `deadline`, moving its result into
/// `joined`; whatever is still blocked at the deadline is left in `pending`.
fn wait_for_readers(
    pending: &mut Vec<(usize, thread::JoinHandle<std::io::Result<String>>)>,
    joined: &mut [Option<std::io::Result<String>>; 2],
    deadline: Instant,
) {
    loop {
        // Take out the readers that finished: `join` on a finished thread
        // cannot block, and removing it keeps `pending` for the stragglers.
        let mut i = 0;
        while i < pending.len() {
            if pending[i].1.is_finished() {
                let (index, handle) = pending.swap_remove(i);
                joined[index] = Some(
                    handle
                        .join()
                        .unwrap_or_else(|_| Err(std::io::Error::other("reader thread panicked"))),
                );
            } else {
                i += 1;
            }
        }
        if pending.is_empty() || Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
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
fn killpg_if_alive(pgid: u32, signal: rustix::process::Signal) -> std::io::Result<()> {
    use rustix::io::Errno;
    use rustix::process::{Pid, kill_process_group, test_kill_process_group};

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
    kill_process_group(pgid, signal)
        .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))
}

enum WaitOutcome {
    /// The child exited; its status is carried from the wait that reaped it
    /// (see [`exit_fields`] for why it must not be re-asked for later).
    Done(std::process::ExitStatus),
    TimedOut,
    WaitFailed(std::io::Error),
}

/// The job's exit information: `(exit, note)`.
///
/// A job that exits normally reports its code. A job killed by a signal —
/// the user's `kill`, the OOM killer, a crashed plugin — has *no* exit
/// code: `ExitStatus::code()` is `None`, and omitting the field would read
/// as "the job is still running" (and let an `exit != 0` check pass). So
/// the signal stands in for the code: `exit` becomes `128+N` (the shell
/// convention, so a SIGKILLed job reads as 137) and the returned note says
/// which signal it was, so the *reason* is not left to be decoded from a
/// number. The note is a diagnostic and joins the job's `stderr`; `error`
/// stays reserved for "the job produced no result at all".
#[cfg(unix)]
fn exit_fields(status: std::process::ExitStatus) -> (Option<i32>, Option<String>) {
    use std::os::unix::process::ExitStatusExt as _;

    if let Some(code) = status.code() {
        return (Some(code), None);
    }
    match status.signal() {
        Some(sig) => {
            let name = rustix::process::Signal::from_raw(sig)
                .map(|s| format!(" ({s:?})"))
                .unwrap_or_default();
            (
                Some(128 + sig),
                Some(format!("killed by signal {sig}{name}")),
            )
        }
        // Neither a code nor a signal: not reachable on Unix, but the honest
        // answer beats inventing a 0.
        None => (
            None,
            Some(format!("exited without an exit code: {status:?}")),
        ),
    }
}

#[cfg(not(unix))]
fn exit_fields(status: std::process::ExitStatus) -> (Option<i32>, Option<String>) {
    (
        status.code(),
        status
            .code()
            .is_none()
            .then(|| format!("exited without an exit code: {status:?}")),
    )
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

    // The status the wait reaped is kept, never re-asked for: a second
    // `child.wait()` after a successful one has no child left to report on.
    let wait_outcome = match timeout {
        Some(t) => match child.wait_timeout(t) {
            Ok(Some(status)) => WaitOutcome::Done(status),
            Ok(None) => WaitOutcome::TimedOut,
            Err(e) => WaitOutcome::WaitFailed(e),
        },
        None => match child.wait() {
            Ok(status) => WaitOutcome::Done(status),
            Err(e) => WaitOutcome::WaitFailed(e),
        },
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
            if let Err(e) = killpg_if_alive(pgid, rustix::process::Signal::Term) {
                kill_note = Some(format!("process-group kill: {e}"));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    // The joins are bounded on every path: the direct child exiting does not
    // mean the pipes are closed — a backgrounded helper it left behind still
    // holds them — so an unbounded `join` would hang the whole batch on a job
    // that has already finished (or been killed). The usual case costs
    // nothing (the readers hit EOF at once); only a genuine straggler waits
    // out the grace, and then the group is signalled to unblock it.
    let [(stdout, stdout_err), (stderr, stderr_err)] =
        join_captures_bounded([out_handle, err_handle], child.id(), JOIN_GRACE);
    // Capture failures, a failed group kill, and a signal death are
    // diagnostics, not data: they are appended to stderr, after whatever the
    // plugin itself wrote and in the order the job hit them.
    let mut stderr_note = stderr;
    let exit_note = match wait_outcome {
        WaitOutcome::Done(status) => exit_fields(status).1,
        _ => None,
    };
    for note in [stdout_err, stderr_err, kill_note, exit_note]
        .into_iter()
        .flatten()
    {
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

    let WaitOutcome::Done(status) = wait_outcome else {
        unreachable!("the timeout and wait-failure paths returned above")
    };
    let (exit, _) = exit_fields(status);
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
            killpg_if_alive(stale_pgid, rustix::process::Signal::Term).is_ok(),
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
        killpg_if_alive(pgid, rustix::process::Signal::Term).expect("live group is signalled");
        let status = child.wait().expect("wait for the signalled child");
        assert_eq!(
            status.signal(),
            Some(15),
            "the child died from SIGTERM: {status:?}"
        );
    }
}
