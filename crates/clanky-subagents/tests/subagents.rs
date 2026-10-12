//! Integration tests: run the `clanky-subagents` binary against a stub
//! `clanky` placed first on PATH. The stub echoes its argv in a stable
//! format and varies behavior by keywords in the instruction.

use std::io::Write;
use std::process::{Command, Output};

const STUB: &str = r#"#!/bin/sh
# Echo argv as lines, then act on keywords in the instruction
# (the last argument).
printf 'ARG %s\n' "$@" | head -c 4000
instr=$(eval "printf '%s' \"\${$#}\"")
case "$instr" in
  *sleep*) sleep 4 ;;
  *fail*) exit 3 ;;
  *stderr*) echo "stub stderr" >&2 ;;
  *big*) head -c 2097152 /dev/zero | tr '\0' 'x' ;;
esac
"#;

static DIR_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A fresh stub dir per call: tests run in parallel within this binary and
/// rewriting a stub file another test is currently executing fails (ETXTBSY).
fn setup() -> std::path::PathBuf {
    let n = DIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("clanky-subagents-test-{}-{n}", std::process::id()));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("clanky"), STUB).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(bin.join("clanky"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    bin
}

fn run(bin: &std::path::Path, input: &str) -> Output {
    let exe = env!("CARGO_BIN_EXE_clanky-subagents");
    Command::new(exe)
        .arg(input)
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .expect("run clanky-subagents")
}

fn jobs_of(out: &Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).expect("valid result JSON")
}

#[test]
fn runs_jobs_in_parallel_and_reports_in_input_order() {
    let bin = setup();
    let input = r#"{"jobs":[
        {"id":"a","instruction":"plain search"},
        {"id":"b","instruction":"plain tests"},
        {"id":"c","instruction":"fail here"}
    ]}"#;
    let out = run(&bin, input);
    assert!(out.status.success());
    let v = jobs_of(&out);
    let ids: Vec<&str> = v["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a", "b", "c"]);
    for j in v["jobs"].as_array().unwrap() {
        assert!(j["stdout"].as_str().unwrap().contains("ARG -pq"));
    }
    assert_eq!(v["jobs"][2]["exit"], serde_json::json!(3));
}

#[test]
fn passes_options_and_defaults_to_argv() {
    let bin = setup();
    let input = r#"{"defaults":{"model":"default-model","thinking":"off"},
      "jobs":[
        {"id":"a","instruction":"job a","model":"own-model"},
        {"id":"b","instruction":"job b"}
    ]}"#;
    let out = run(&bin, input);
    assert!(out.status.success());
    let v = jobs_of(&out);
    let a = v["jobs"][0]["stdout"].as_str().unwrap();
    assert!(a.contains("ARG --model"), "{a}");
    assert!(a.contains("own-model"), "{a}");
    assert!(a.contains("ARG --thinking"), "{a}");
    assert!(a.contains("off"), "{a}");
    let b = v["jobs"][1]["stdout"].as_str().unwrap();
    assert!(b.contains("default-model"), "{b}");
    assert!(b.contains("job b"), "{b}");
}

#[test]
fn parallel_jobs_overlap_in_time() {
    let bin = setup();
    // Two 4-second jobs at the default max_parallel (= job count) must
    // finish in ~4s, not ~8s, if they actually overlapped.
    let input = r#"{"jobs":[
        {"id":"a","instruction":"slow sleep job a"},
        {"id":"b","instruction":"slow sleep job b"}
    ]}"#;
    let start = std::time::Instant::now();
    let out = run(&bin, input);
    assert!(out.status.success());
    assert!(
        start.elapsed().as_secs() < 7,
        "jobs did not overlap: {:?}",
        start.elapsed()
    );
}

#[test]
fn timeout_kills_job_and_reports_partial_output() {
    let bin = setup();
    let input = r#"{"timeout_secs":1,"jobs":[
        {"id":"a","instruction":"sleep a lot"}
    ]}"#;
    let start = std::time::Instant::now();
    let out = run(&bin, input);
    assert!(start.elapsed().as_secs() < 20);
    assert!(out.status.success());
    let v = jobs_of(&out);
    let j = &v["jobs"][0];
    assert_eq!(j["error"].as_str().unwrap(), "timeout after 1s");
    // The stub echoes its argv before sleeping, so partial stdout exists.
    assert!(j["stdout"].as_str().unwrap().contains("ARG -pq"));
}

#[test]
fn duplicate_ids_fail_with_nonzero_exit() {
    let bin = setup();
    let out = run(
        &bin,
        r#"{"jobs":[{"id":"a","instruction":"x"},{"id":"a","instruction":"y"}]}"#,
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("duplicate job id `a`"));
}

#[test]
fn empty_jobs_fail() {
    let bin = setup();
    let out = run(&bin, r#"{"jobs":[]}"#);
    assert!(!out.status.success());
}

#[test]
fn unknown_field_named_in_error() {
    let bin = setup();
    let out = run(&bin, r#"{"jobs":[{"id":"a","instruction":"x","bogus":1}]}"#);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("bogus"), "{stderr}");
}

#[test]
fn denied_options_cannot_appear_in_argv() {
    let bin = setup();
    // A job id or instruction crafted to look like a flag is still just
    // the instruction value; argv only ever contains the allow-list.
    let input = r#"{"jobs":[{"id":"a","instruction":"--resume"}]}"#;
    let out = run(&bin, input);
    assert!(out.status.success());
    let v = jobs_of(&out);
    let stdout = v["jobs"][0]["stdout"].as_str().unwrap();
    // The instruction is passed as a single argv entry: "--resume" appears
    // once, after -pq, never as a parsed flag (the stub shows one ARG line).
    assert_eq!(stdout.matches("--resume").count(), 1);
}

#[test]
fn reads_input_from_stdin() {
    let bin = setup();
    let exe = env!("CARGO_BIN_EXE_clanky-subagents");
    let mut child = Command::new(exe)
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"jobs":[{"id":"a","instruction":"from stdin"}]}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("from stdin"));
}

#[test]
fn oversized_stream_keeps_tail_with_marker() {
    let bin = setup();
    let input = r#"{"jobs":[{"id":"a","instruction":"big output"}]}"#;
    let out = run(&bin, input);
    assert!(out.status.success());
    let v = jobs_of(&out);
    let stdout = v["jobs"][0]["stdout"].as_str().unwrap();
    assert!(
        stdout.starts_with("[output truncated, kept last 1 MiB]"),
        "{stdout:.100}"
    );
    // Roughly the tail: no more than cap + marker + lossy slack.
    assert!(
        stdout.len() < MAX_STREAM_BYTES + 1_000_000 / 10,
        "{}",
        stdout.len()
    );
}

const MAX_STREAM_BYTES: usize = 1024 * 1024;

#[test]
fn missing_clanky_reports_spawn_error_per_job() {
    let dir = std::env::temp_dir().join(format!("clanky-subagents-empty-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_clanky-subagents"))
        .arg(r#"{"jobs":[{"id":"a","instruction":"x"}]}"#)
        .env("PATH", dir.display().to_string())
        .output()
        .unwrap();
    assert!(out.status.success(), "batch still exits 0");
    let v = jobs_of(&out);
    assert!(
        v["jobs"][0]["error"]
            .as_str()
            .unwrap()
            .starts_with("spawn:")
    );
}
