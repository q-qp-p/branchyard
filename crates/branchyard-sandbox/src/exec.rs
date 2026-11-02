//! Piped execution through an operator-controlled transport.
//!
//! The transport reaches an already authenticated allocation. This module
//! quotes argument vectors, waits for a guest startup acknowledgement and
//! signals the process group it records there. A guest can escape that group;
//! a provider's stop/destroy must still remove the whole allocation. Guest
//! records are bookkeeping, not an isolation or authentication boundary.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStderr, Command, ExitStatus as StdStatus, Stdio};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use crate::{ExecSpec, ExitStatus, Process, ProviderError};

const STARTED: &str = "by-sandbox-started";
const MISSING: &str = "by-sandbox-missing";
const MAX_START_OUTPUT: u64 = 64 * 1024;

// Preserve the recipe adapter's record directory across this extraction.
const OUTER: &str = r#"p=$(ps -o pgid= -p $$ 2>/dev/null | tr -d ' ')
if [ "$p" != "$$" ] && command -v setsid >/dev/null 2>&1; then exec setsid sh -c "$1"; fi
exec sh -c "$1""#;
const STOP: &str = r#"for f in "$HOME/.branchyard/recipe-exec/$1"/*; do
  [ -f "$f" ] || continue
  kill -s KILL -- $(cat "$f") 2>/dev/null
  rm -f "$f"
done
:"#;
const TEARDOWN: &str = r#"f="$HOME/.branchyard/recipe-exec/$1"
t=$(cat "$f" 2>/dev/null) || exit 0
case $t in
  -*) ps -eo pgid=,comm= 2>/dev/null | awk -v g="${t#-}" '$1 == g { print $2 }' ;;
  *) ps -o comm= -p "$t" 2>/dev/null ;;
esac
kill -s KILL -- $t 2>/dev/null
rm -f "$f"
:"#;
const KILL: &str = r#"t=$(cat "$HOME/.branchyard/recipe-exec/$1" 2>/dev/null) && kill -s KILL -- $t 2>/dev/null
:"#;
const FORGET: &str = r#"rm -f "$HOME/.branchyard/recipe-exec/$1"
:"#;

/// A trusted allocation's way in. Implementations must bind every command
/// to the allocation's immutable identity before executing its script.
pub trait ExecTransport: Send + Sync {
    /// Run `script` with `args` as `$1`, `$2`, ... and piped standard I/O.
    /// This is an internal command builder, never an API request type.
    fn command(&self, script: &str, args: &[&str]) -> Command;

    /// A human-readable transport name for diagnostics.
    fn describe(&self) -> String;

    /// Bound startup acknowledgement and control commands, not the workload.
    fn start_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }
}

/// The safe guest record component that groups an allocation's execs.
#[derive(Clone, Debug)]
pub struct ExecGroup(String);

impl ExecGroup {
    /// Reject empty, hidden or non-component names rather than aliasing them.
    pub fn new(name: impl Into<String>) -> Result<Self, ProviderError> {
        let name = name.into();
        if name.is_empty()
            || name.starts_with('.')
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(ProviderError::Invalid("invalid exec group name".into()));
        }
        Ok(Self(name))
    }
}

fn quote(text: &str) -> Result<String, ProviderError> {
    shlex::try_quote(text)
        .map(|word| word.into_owned())
        .map_err(|_| ProviderError::Invalid("an exec value contains a NUL byte".into()))
}

/// Start an argument vector on an authenticated allocation without a TTY.
/// Failure to find the program, enter its directory or acknowledge startup
/// is an error from this call, before a `Process` is returned.
pub fn spawn(
    transport: Arc<dyn ExecTransport>,
    group: &ExecGroup,
    spec: &ExecSpec,
) -> Result<Box<dyn Process>, ProviderError> {
    spec.refuse_write_scope()?;
    let Some(program) = spec.argv.first().filter(|program| !program.is_empty()) else {
        return Err(ProviderError::Invalid(
            "the argument vector is empty".into(),
        ));
    };
    let id = branchyard_support::new_ulid().map_err(io::Error::from)?;
    let record = format!("{}/{id}", group.0);
    let cwd = if spec.cwd.as_os_str().is_empty() {
        ".".into()
    } else {
        spec.cwd
            .to_str()
            .ok_or_else(|| ProviderError::Invalid("exec cwd is not UTF-8".into()))?
            .to_owned()
    };
    let mut inner = format!(
        "d=\"$HOME/.branchyard/recipe-exec/{dir}\"; mkdir -p \"$d\" || exit 125\n\
         p=$(ps -o pgid= -p $$ 2>/dev/null | tr -d ' ')\n\
         if [ \"$p\" = \"$$\" ]; then echo \"-$$\" >\"$d/{id}\"; else echo \"$$\" >\"$d/{id}\"; fi\n\
         cd {cwd} 2>/dev/null || {{ echo 'cannot enter exec directory' >&2; exit 125; }}\n",
        dir = group.0,
        cwd = quote(&cwd)?,
    );
    for (name, value) in &spec.env {
        let name = name
            .to_str()
            .ok_or_else(|| ProviderError::Invalid("exec variable name is not UTF-8".into()))?;
        let value = value
            .to_str()
            .ok_or_else(|| ProviderError::Invalid("exec variable value is not UTF-8".into()))?;
        if !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(ProviderError::Invalid("invalid exec variable name".into()));
        }
        inner.push_str(&format!("export {}\n", quote(&format!("{name}={value}"))?));
    }
    let quoted_program = quote(program)?;
    inner.push_str(&format!(
        "case {quoted_program} in */*) [ -f {quoted_program} ] && [ -x {quoted_program} ] ;; \
         *) command -v {quoted_program} >/dev/null 2>&1 ;; esac || \
         {{ echo {MISSING} >&2; exit 127; }}\n\
         echo {STARTED} >&2\nexec"
    ));
    for arg in &spec.argv {
        inner.push(' ');
        inner.push_str(&quote(arg)?);
    }
    let mut command = transport.command(OUTER, &[&inner]);
    process_group(&mut command);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let started = startup(&mut child, transport.start_timeout());
    let (stderr, before) = match started {
        Ok(started) => started,
        Err(error) => {
            branchyard_support::kill_group(child.id());
            branchyard_support::best_effort("kill failed exec startup", child.kill());
            branchyard_support::best_effort("reap failed exec startup", child.wait());
            return Err(ProviderError::Io(io::Error::new(
                error.kind(),
                format!(
                    "could not start {program} through {}: {error}",
                    transport.describe()
                ),
            )));
        }
    };
    let stderr: Box<dyn Read + Send> = Box::new(io::Cursor::new(before).chain(stderr));
    Ok(Box::new(RemoteProcess {
        transport,
        record,
        stdin: child
            .stdin
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Write + Send>),
        stdout: child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
        stderr: Some(stderr),
        child,
        status: None,
        torn_down: false,
    }))
}

fn process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

fn startup(child: &mut Child, timeout: Duration) -> io::Result<(BufReader<ChildStderr>, Vec<u8>)> {
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("exec stderr is not piped"))?;
    let (send, receive) = mpsc::channel();
    let reader = branchyard_support::spawn_named(
        "sandbox-exec-start",
        |_| {},
        move || {
            branchyard_support::best_effort(
                "deliver sandbox startup result",
                send.send(read_start(stderr)),
            );
        },
    )?;
    let result = receive.recv_timeout(timeout).map_err(|error| {
        io::Error::new(
            if matches!(error, mpsc::RecvTimeoutError::Timeout) {
                io::ErrorKind::TimedOut
            } else {
                io::ErrorKind::Other
            },
            "transport did not acknowledge exec startup",
        )
    });
    if result.is_err() {
        branchyard_support::kill_group(child.id());
        branchyard_support::best_effort("kill unacknowledged exec", child.kill());
        branchyard_support::best_effort("reap unacknowledged exec", child.wait());
    }
    branchyard_support::join_reporting("sandbox exec startup", reader);
    result?
}

fn read_start(stderr: ChildStderr) -> io::Result<(BufReader<ChildStderr>, Vec<u8>)> {
    let mut reader = BufReader::new(stderr);
    let mut before = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let remaining = MAX_START_OUTPUT.saturating_sub(before.len() as u64);
        if remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exec startup output exceeds its limit",
            ));
        }
        if (&mut reader).take(remaining).read_until(b'\n', &mut line)? == 0 {
            let missing = before
                .split(|byte| *byte == b'\n')
                .any(|line| line == MISSING.as_bytes());
            return Err(io::Error::new(
                if missing {
                    io::ErrorKind::NotFound
                } else {
                    io::ErrorKind::Other
                },
                if missing {
                    "guest program was not found".to_owned()
                } else {
                    String::from_utf8_lossy(&before).trim().to_owned()
                },
            ));
        }
        if line.trim_ascii_end() == STARTED.as_bytes() {
            return Ok((reader, before));
        }
        before.extend_from_slice(&line);
    }
}

/// Signal the process groups this allocation recorded. This does not replace
/// provider-level stop/destroy and cannot reach processes that escaped a group.
pub fn stop_group(transport: &dyn ExecTransport, group: &ExecGroup) -> io::Result<()> {
    control(transport, STOP, &[&group.0]).map(|_| ())
}

fn control(transport: &dyn ExecTransport, script: &str, args: &[&str]) -> io::Result<String> {
    let mut command = transport.command(script, args);
    process_group(&mut command);
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id();
    let (send, receive) = mpsc::channel();
    let waiter = branchyard_support::spawn_named(
        "sandbox-exec-control",
        |_| {},
        move || {
            branchyard_support::best_effort(
                "deliver sandbox control result",
                send.send(child.wait_with_output()),
            );
        },
    )?;
    let output = receive
        .recv_timeout(transport.start_timeout())
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "exec control command did not finish",
            )
        });
    if output.is_err() {
        branchyard_support::kill_group(pid);
    }
    branchyard_support::join_reporting("sandbox exec control", waiter);
    let output = output??;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "exec control failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

struct RemoteProcess {
    transport: Arc<dyn ExecTransport>,
    record: String,
    child: Child,
    stdin: Option<Box<dyn Write + Send>>,
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
    status: Option<ExitStatus>,
    torn_down: bool,
}

impl RemoteProcess {
    fn remote(&self, script: &str) -> io::Result<String> {
        control(self.transport.as_ref(), script, &[&self.record])
    }

    fn kill_local(&mut self) {
        branchyard_support::kill_group(self.child.id());
        branchyard_support::best_effort("kill exec transport", self.child.kill());
    }
}

fn status(status: StdStatus) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitStatus {
            code: status.code(),
            signal: status.signal(),
        }
    }
    #[cfg(not(unix))]
    {
        ExitStatus {
            code: status.code(),
            signal: None,
        }
    }
}

impl Process for RemoteProcess {
    fn id(&self) -> String {
        format!("{} via {}", self.record, self.transport.describe())
    }
    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        self.stdin.take()
    }
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stdout.take()
    }
    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stderr.take()
    }
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?.map(status);
        }
        Ok(self.status)
    }
    fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let waited = status(self.child.wait()?);
        self.status = Some(waited);
        Ok(waited)
    }
    fn kill(&mut self) -> io::Result<()> {
        self.remote(KILL)?;
        self.kill_local();
        Ok(())
    }
    fn teardown(&mut self) -> Vec<String> {
        self.torn_down = true;
        let names = match self.remote(TEARDOWN) {
            Ok(names) => names
                .lines()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect(),
            Err(error) => vec![format!("could not tear down exec group: {error}")],
        };
        self.kill_local();
        branchyard_support::best_effort(
            "check whether exec transport exited",
            self.child.try_wait(),
        );
        names
    }
}

impl Drop for RemoteProcess {
    fn drop(&mut self) {
        if self.torn_down {
            return;
        }
        match self.try_wait() {
            Ok(Some(_)) => {
                branchyard_support::best_effort("forget completed exec", self.remote(FORGET));
            }
            _ => {
                branchyard_support::best_effort("kill abandoned exec group", self.remote(KILL));
                self.teardown();
                branchyard_support::best_effort("reap abandoned exec transport", self.child.wait());
            }
        }
    }
}
