//! The adapter protocol: how `aether-conform run` asks a director under test
//! to mount a scenario's tree and to let it go again.
//!
//! An adapter is a program the run starts as a child process
//! (`--director "<command line>"`). It talks JSON, one object per line: the
//! run writes one request on the adapter's stdin and reads one answer from
//! its stdout, and never sends the next request before the answer came.
//!
//! ```text
//! {"op":"mount","scenario":"<absolute path of the scenario file>"}
//!     -> {"ok":true,"env":{"VFS_RING_PATH":"...","VFS_RING_BYTES":"...",...}}
//!     or {"ok":false,"error":"..."}
//! {"op":"unmount"}
//!     -> {"ok":true}
//! ```
//!
//! `env` holds the handshake variables (`docs/wire.md` 5.3) a shim launched
//! into that tree would get; the run attaches to the ring they name exactly
//! as the shim does ([`crate::attach`]). EOF on stdin means the run is over
//! and the adapter should exit. Whatever the adapter writes on stderr is
//! kept, and its last lines are shown when something goes wrong.
//!
//! Every answer has a deadline. A crash, EOF on stdout, a line that is not a
//! JSON object of the right shape, or no answer by the deadline is an
//! [`AdapterError`]: it fails the scenario being run and ends the run, since
//! nothing more can be asked of that adapter.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

/// How many of the adapter's last stderr lines an error shows.
const STDERR_TAIL: usize = 20;

/// The handshake variables a mount answered with.
pub type Env = BTreeMap<String, String>;

/// Why the adapter can be asked nothing more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterError {
    /// What happened.
    pub what: String,
    /// The adapter's last stderr lines.
    pub stderr: Vec<String>,
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "adapter: {}", self.what)?;
        if self.stderr.is_empty() {
            write!(f, " (nothing on its stderr)")
        } else {
            write!(f, "; its stderr ended with:")?;
            for l in &self.stderr {
                write!(f, "\n  | {l}")?;
            }
            Ok(())
        }
    }
}

impl std::error::Error for AdapterError {}

/// A running adapter.
pub struct Adapter {
    /// The child and every process it starts, killed together.
    tree: ProcessTree,
    stdin: Option<ChildStdin>,
    lines: Receiver<Option<std::io::Result<String>>>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    timeout: Duration,
}

impl Adapter {
    /// Starts `command_line` ([`split_command_line`]) with piped stdio.
    /// `timeout` bounds every answer.
    pub fn start(command_line: &str, timeout: Duration) -> Result<Adapter, AdapterError> {
        let words = split_command_line(command_line);
        let Some((program, args)) = words.split_first() else {
            return Err(AdapterError {
                what: "the director command line is empty".into(),
                stderr: Vec::new(),
            });
        };
        let mut cmd = Command::new(program);
        cmd.args(args);
        Adapter::spawn(cmd, program, timeout)
    }

    /// Starts `cmd` with piped stdio, as [`Adapter::start`] does a command
    /// line: any child that answers one JSON line per JSON line, as the
    /// client workers do ([`crate::clients`]). `program` names it in errors.
    pub fn spawn(
        mut cmd: Command,
        program: &str,
        timeout: Duration,
    ) -> Result<Adapter, AdapterError> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut child, tree) = ProcessTree::spawn(&mut cmd).map_err(|e| AdapterError {
            what: format!("cannot start {program:?}: {e}"),
            stderr: Vec::new(),
        })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr_pipe = child.stderr.take().expect("piped stderr");

        // Both pipes are drained on threads of their own, so an adapter that
        // writes a lot on one is never blocked while the run reads the other,
        // and a read of stdout can time out. Neither thread is ever joined:
        // a process the adapter started may hold the pipes open, and the run
        // must not wait for it. Killing the tree closes them.
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let failed = line.is_err();
                if tx.send(Some(line)).is_err() || failed {
                    return;
                }
            }
            let _ = tx.send(None);
        });
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        let tail = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines() {
                let Ok(line) = line else { return };
                let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                if t.len() == STDERR_TAIL {
                    t.pop_front();
                }
                t.push_back(line);
            }
        });
        let tree = tree.holding(child);
        Ok(Adapter {
            tree,
            stdin,
            lines,
            stderr,
            timeout,
        })
    }

    /// Asks the adapter to mount the scenario at `scenario`. `Ok(Err(why))`
    /// is a refusal (`{"ok":false}`), which fails only that scenario.
    pub fn mount(&mut self, scenario: &Path) -> Result<Result<Env, String>, AdapterError> {
        let scenario = std::path::absolute(scenario).unwrap_or_else(|_| scenario.to_path_buf());
        let answer = self.ask(&json!({"op": "mount", "scenario": scenario.to_string_lossy()}))?;
        match answer.get("ok") {
            Some(Value::Bool(true)) => {
                let Some(Value::Object(env)) = answer.get("env") else {
                    return Err(self.error(format!(
                        "a mount answer with \"ok\":true has no \"env\" object: {answer}"
                    )));
                };
                let mut out = Env::new();
                for (k, v) in env {
                    let Value::String(v) = v else {
                        return Err(self.error(format!(
                            "\"env\".{k} is not a string in the mount answer: {answer}"
                        )));
                    };
                    out.insert(k.clone(), v.clone());
                }
                Ok(Ok(out))
            }
            Some(Value::Bool(false)) => Ok(Err(refusal(&answer))),
            _ => Err(self.error(format!("the mount answer has no boolean \"ok\": {answer}"))),
        }
    }

    /// Asks the adapter to let the mounted tree go. A refusal is an error
    /// here: the next mount could not be trusted.
    pub fn unmount(&mut self) -> Result<(), AdapterError> {
        let answer = self.ask(&json!({"op": "unmount"}))?;
        match answer.get("ok") {
            Some(Value::Bool(true)) => Ok(()),
            Some(Value::Bool(false)) => {
                Err(self.error(format!("unmount refused: {}", refusal(&answer))))
            }
            _ => Err(self.error(format!(
                "the unmount answer has no boolean \"ok\": {answer}"
            ))),
        }
    }

    /// Ends the session: EOF on the adapter's stdin, then up to the timeout
    /// for it to exit. Then whatever is left of its process tree, the
    /// adapter included if it has not exited, is killed.
    pub fn finish(mut self) {
        self.stdin = None;
        let deadline = std::time::Instant::now() + self.timeout;
        while std::time::Instant::now() < deadline {
            if self.tree.exited().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Drop kills the tree.
    }

    /// Sends `request` as one line and returns the JSON object answered,
    /// within the timeout: the protocol's one round trip, for a child that
    /// is not a mount adapter ([`Adapter::spawn`]).
    pub fn request(&mut self, request: &Value) -> Result<Value, AdapterError> {
        self.ask(request)
    }

    /// Kills the child and every process it started now, and reaps it: a
    /// process that is killed, as a game's processes may be (`docs/wire.md`
    /// 2.8.1). A tree that could not be killed is returned.
    pub fn kill_now(&mut self) -> Option<String> {
        self.kill()
    }

    /// Kills the adapter and every process it started. A tree that could
    /// not be killed is said on stderr and returned.
    fn kill(&mut self) -> Option<String> {
        let failed = self.tree.kill().err();
        if let Some(e) = &failed {
            eprintln!("aether-conform: {e}");
        }
        failed
    }

    fn ask(&mut self, request: &Value) -> Result<Value, AdapterError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(self.error("its stdin is closed".into()));
        };
        let line = format!("{request}\n");
        if let Err(e) = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.flush())
        {
            return Err(self.gone(format!("cannot write to its stdin ({e})")));
        }
        let line = match self.lines.recv_timeout(self.timeout) {
            Ok(Some(Ok(line))) => line,
            Ok(Some(Err(e))) => return Err(self.gone(format!("reading its stdout failed: {e}"))),
            Ok(None) | Err(RecvTimeoutError::Disconnected) => {
                return Err(self.gone("its stdout ended (EOF) before it answered".into()))
            }
            Err(RecvTimeoutError::Timeout) => {
                let secs = self.timeout.as_secs_f64();
                let mut what = format!("no answer within {secs} s to {request}");
                if let Some(e) = self.kill() {
                    what.push_str(&format!("; and {e}"));
                }
                return Err(self.error(what));
            }
        };
        match serde_json::from_str::<Value>(&line) {
            Ok(v @ Value::Object(_)) => Ok(v),
            Ok(_) => Err(self.error(format!("its answer is not a JSON object: {line:?}"))),
            Err(e) => Err(self.error(format!("its answer is not JSON ({e}): {line:?}"))),
        }
    }

    /// An error for an adapter whose pipes broke: says whether it exited,
    /// and with what.
    fn gone(&mut self, what: String) -> AdapterError {
        // A crashed child closes its pipes just before the OS reaps it.
        let mut status = None;
        for _ in 0..50 {
            if let Some(s) = self.tree.exited() {
                status = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let what = match status {
            Some(s) => format!("{what}; it exited ({s})"),
            None => what,
        };
        self.error(what)
    }

    fn error(&mut self, what: String) -> AdapterError {
        // The stderr thread may still be taking the last lines in.
        std::thread::sleep(Duration::from_millis(50));
        let stderr = self
            .stderr
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        AdapterError { what, stderr }
    }
}

impl Drop for Adapter {
    fn drop(&mut self) {
        // Even an adapter that exited may have left processes behind.
        let _ = self.kill();
    }
}

/// The adapter's process tree.
///
/// On Windows it is a Job Object that kills its members when closed
/// (`crate::job::KillOnCloseJob`); the adapter is created suspended and
/// assigned before it runs, so every process it starts is in the job. On
/// Unix the adapter leads a new process group, and the group is sent
/// `SIGKILL`. A process that leaves the group (`setsid`) is not followed.
///
/// A process group's number is free for reuse once its last member is
/// reaped, so on Unix the group is killed while the leader is still
/// unreaped: alive, or a zombie. On Linux [`ProcessTree::exited`] sees the
/// zombie in `/proc` and kills the group before reaping it. Where there is
/// no `/proc` it has to reap to learn of the exit, and then the group is
/// not signalled at all, since its number may already be another's: the
/// processes an adapter left behind after exiting by itself survive there.
struct ProcessTree {
    child: Option<Child>,
    /// The leader's exit, once reaped.
    status: Option<std::process::ExitStatus>,
    #[cfg(windows)]
    job: crate::job::KillOnCloseJob,
    #[cfg(unix)]
    group: u32,
    /// Whether the group has been sent SIGKILL (or must not be, being
    /// possibly reused).
    #[cfg(unix)]
    group_done: bool,
}

impl ProcessTree {
    #[cfg(windows)]
    fn spawn(cmd: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
        let job = crate::job::KillOnCloseJob::new()?;
        let child = job.spawn(cmd)?;
        let tree = ProcessTree {
            child: None,
            status: None,
            job,
        };
        Ok((child, tree))
    }

    #[cfg(unix)]
    fn spawn(cmd: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
        use std::os::unix::process::CommandExt;
        let child = cmd.process_group(0).spawn()?;
        let tree = ProcessTree {
            child: None,
            status: None,
            group: child.id(),
            group_done: false,
        };
        Ok((child, tree))
    }

    #[cfg(not(any(windows, unix)))]
    fn spawn(cmd: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
        let tree = ProcessTree {
            child: None,
            status: None,
        };
        Ok((cmd.spawn()?, tree))
    }

    /// The tree, owning its leader.
    fn holding(mut self, child: Child) -> ProcessTree {
        self.child = Some(child);
        self
    }

    /// The leader's exit status, if it has exited; on Unix the group is
    /// killed before the leader is reaped (see the type's doc).
    fn exited(&mut self) -> Option<std::process::ExitStatus> {
        if self.status.is_some() {
            return self.status;
        }
        #[cfg(target_os = "linux")]
        {
            let pid = self.group;
            let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map(|stat| {
                    stat.rsplit_once(')')
                        .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
                })
                .unwrap_or(false);
            if !zombie {
                return None;
            }
            if let Err(e) = self.kill_group() {
                eprintln!("aether-conform: {e}");
            }
        }
        let child = self.child.as_mut()?;
        if let Ok(Some(s)) = child.try_wait() {
            self.status = Some(s);
            #[cfg(unix)]
            {
                // Reaped: the group's number may be another's from now on.
                self.group_done = true;
            }
        }
        self.status
    }

    /// Sends SIGKILL to the group, once, while its leader is unreaped.
    #[cfg(unix)]
    fn kill_group(&mut self) -> Result<(), String> {
        if self.group_done {
            return Ok(());
        }
        self.group_done = true;
        // There is no libc here (the crate denies unsafe code outside its
        // Windows job module), so kill(1) sends it.
        let group = format!("-{}", self.group);
        match Command::new("kill")
            .args(["-KILL", "--", &group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
        {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(format!(
                "killing the adapter's process group with `kill -KILL -- {group}` failed ({}): {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => Err(format!(
                "cannot run `kill -KILL -- {group}` to kill the adapter's process group: {e}; \
                 processes it started may still be running"
            )),
        }
    }

    /// Kills every process in the tree, and reaps the leader.
    fn kill(&mut self) -> Result<(), String> {
        #[cfg(windows)]
        self.job.terminate();
        #[cfg(unix)]
        let group = if self.status.is_none() {
            self.kill_group()
        } else {
            Ok(())
        };
        #[cfg(not(unix))]
        let group = Ok(());
        if self.status.is_none() {
            if let Some(child) = self.child.as_mut() {
                if let Ok(None) = child.try_wait() {
                    let _ = child.kill();
                }
                self.status = child.wait().ok();
            }
        }
        group
    }
}

fn refusal(answer: &Value) -> String {
    match answer.get("error") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "(no \"error\" given)".into(),
    }
}

/// Splits a command line into words the same way on every OS: whitespace
/// separates words, double quotes group, and a backslash is literal except
/// before a double quote, where it makes the quote literal. So a Windows
/// path needs no doubled backslashes, and one with spaces is quoted.
pub fn split_command_line(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'"') => {
                word.push('"');
                chars.next();
                in_word = true;
            }
            '"' => {
                quoted = !quoted;
                in_word = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// A request as an adapter receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterRequest {
    Mount { scenario: String },
    Unmount,
}

/// The adapter's side of the protocol: reads requests from stdin until EOF,
/// answers each with `answer`, and writes the answer as one line on stdout.
/// A line that is not a request is answered `{"ok":false}` with the reason.
pub fn answer_requests(mut answer: impl FnMut(AdapterRequest) -> Value) {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match parse_request(&line) {
            Ok(req) => answer(req),
            Err(e) => json!({"ok": false, "error": e}),
        };
        if writeln!(stdout, "{reply}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            return;
        }
    }
}

/// Parses one request line.
pub fn parse_request(line: &str) -> Result<AdapterRequest, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
    match v.get("op").and_then(Value::as_str) {
        Some("mount") => match v.get("scenario").and_then(Value::as_str) {
            Some(s) => Ok(AdapterRequest::Mount {
                scenario: s.to_string(),
            }),
            None => Err("a mount names no \"scenario\"".into()),
        },
        Some("unmount") => Ok(AdapterRequest::Unmount),
        Some(op) => Err(format!("unknown op {op:?}")),
        None => Err("no \"op\"".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_split_on_spaces_outside_quotes() {
        assert_eq!(
            split_command_line(r#""C:\Program Files\d.exe" adapter --ring section"#),
            [r"C:\Program Files\d.exe", "adapter", "--ring", "section"]
        );
        assert_eq!(
            split_command_line(r#"  java -jar d.jar  "a \"b\"" "" "#),
            ["java", "-jar", "d.jar", r#"a "b""#, ""]
        );
        assert!(split_command_line("   ").is_empty());
    }

    #[test]
    fn requests_parse() {
        assert_eq!(
            parse_request(r#"{"op":"mount","scenario":"/s/a.json"}"#),
            Ok(AdapterRequest::Mount {
                scenario: "/s/a.json".into()
            })
        );
        assert_eq!(
            parse_request(r#"{"op":"unmount"}"#),
            Ok(AdapterRequest::Unmount)
        );
        assert!(parse_request(r#"{"op":"dance"}"#).is_err());
        assert!(parse_request("nope").is_err());
    }
}
