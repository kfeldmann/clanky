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
use std::time::Duration;

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

/// Kill a process group without the `libc` crate. Runs `kill -pgid -TERM`
/// via /bin/sh; if that fails the direct-child kill below is the fallback.
#[cfg(unix)]
fn unsafe_killpg(pgid: i32) -> std::io::Result<()> {
    let status = Command::new("kill")
        .args(["-TERM", &format!("-{pgid}")])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "kill -TERM -{pgid} exited with {status:?}"
        )))
    }
}

enum WaitOutcome {
    Done,
    TimedOut,
    WaitFailed(std::io::Error),
}

fn run_job(job: &Job, defaults: &Options, timeout: Option<Duration>) -> JobResult {
    let mut args = vec!["-pq".to_string()];
    effective(&job.options, defaults).push_args(&mut args);
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
    if timed_out {
        // Kill before joining the capture threads: wait_timeout leaves the
        // child running, and its grandchildren inherit the pipes — the joins
        // would block until they exit. Killing the job's whole process
        // group (set at spawn) makes everyone hit EOF.
        #[cfg(unix)]
        {
            let pgid = child.id() as i32;
            let _ = unsafe_killpg(pgid);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let stdout = out_handle.join().unwrap_or_else(|_| Ok(String::new()));
    let stderr = err_handle.join().unwrap_or_else(|_| Ok(String::new()));
    let (stdout, stderr) = match (stdout, stderr) {
        (Ok(o), Err(e)) => (o, format!("capture stderr: {e}")),
        (Err(e), Ok(e2)) => (format!("capture stdout: {e}"), e2),
        (Ok(o), Ok(e)) => (o, e),
        (Err(a), Err(b)) => (
            format!("capture stdout: {a}"),
            format!("capture stderr: {b}"),
        ),
    };

    if timed_out {
        return JobResult {
            id: job.id.clone(),
            exit: None,
            stdout: Some(stdout),
            stderr: Some(stderr),
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
        stderr: Some(stderr),
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
