//! The shared exec contract through a local stand-in for SSH/container exec.
//! No cloud, sandbox runtime or model is used.

#![cfg(unix)]
#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
#![allow(clippy::panic)] // tests: a panic is the failure report

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use branchyard_sandbox::exec::{self, ExecGroup, ExecTransport};
use branchyard_sandbox::{ExecSpec, ProviderError};
use branchyard_testkit::{wait, Scratch};

struct LocalTransport {
    home: PathBuf,
    tools: PathBuf,
    acknowledge: bool,
}

impl ExecTransport for LocalTransport {
    fn command(&self, script: &str, args: &[&str]) -> Command {
        let mut command = Command::new(self.tools.join("sh"));
        branchyard_testkit::hermetic(&mut command);
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", &self.tools)
            .args([
                "-c",
                if self.acknowledge {
                    script
                } else {
                    "cat >/dev/null"
                },
                "sh",
            ])
            .args(args);
        command
    }

    fn describe(&self) -> String {
        "local test transport".into()
    }

    fn start_timeout(&self) -> Duration {
        if self.acknowledge {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(10)
        }
    }
}

struct Fixture {
    root: Scratch,
    transport: Arc<LocalTransport>,
    group: ExecGroup,
}

impl Fixture {
    fn new() -> Fixture {
        let root = Scratch::new("sandbox-exec");
        let home = root.join("home");
        let tools = root.join("bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(root.join("work")).unwrap();
        branchyard_testkit::system_tools(&tools).unwrap();
        for tool in ["ps", "awk", "setsid", "mkfifo"] {
            std::os::unix::fs::symlink(format!("/usr/bin/{tool}"), tools.join(tool)).unwrap();
        }
        Fixture {
            root,
            transport: Arc::new(LocalTransport {
                home,
                tools,
                acknowledge: true,
            }),
            group: ExecGroup::new("allocation-1").unwrap(),
        }
    }

    fn spec(&self, argv: &[&str]) -> ExecSpec {
        ExecSpec {
            argv: argv.iter().map(|arg| (*arg).into()).collect(),
            cwd: self.root.join("work"),
            ..ExecSpec::default()
        }
    }
}

#[test]
fn stdio_environment_cwd_and_literal_arguments_survive_the_transport() {
    let fixture = Fixture::new();
    let literal = "$(touch should-not-exist); 'quoted'";
    let mut spec = fixture.spec(&[
        "sh", "-c", "printf '%s:%s\\n' \"$VALUE\" \"$PWD\"; printf '%s\\n' \"$1\"; cat; printf 'warning\\000bytes' >&2",
        "sh", literal,
    ]);
    spec.env
        .insert("VALUE".into(), "a value ' with quotes".into());
    let mut process = exec::spawn(fixture.transport.clone(), &fixture.group, &spec).unwrap();
    let mut input = process.take_stdin().unwrap();
    input.write_all(b"binary\0input\n").unwrap();
    drop(input);
    let mut output = Vec::new();
    process
        .take_stdout()
        .unwrap()
        .read_to_end(&mut output)
        .unwrap();
    let mut error = Vec::new();
    process
        .take_stderr()
        .unwrap()
        .read_to_end(&mut error)
        .unwrap();
    assert_eq!(process.wait().unwrap().code, Some(0));
    let mut expected = format!(
        "a value ' with quotes:{}\n{literal}\n",
        fixture.root.join("work").display()
    )
    .into_bytes();
    expected.extend_from_slice(b"binary\0input\n");
    assert_eq!(output, expected);
    assert_eq!(error, b"warning\0bytes");
    assert!(!fixture.root.join("work/should-not-exist").exists());
    process.teardown();
}

#[test]
fn missing_program_and_directory_fail_before_returning_a_process() {
    let fixture = Fixture::new();
    let error = match exec::spawn(
        fixture.transport.clone(),
        &fixture.group,
        &fixture.spec(&["by-program-that-does-not-exist"]),
    ) {
        Err(error) => error,
        Ok(_) => panic!("missing executable was admitted"),
    };
    assert!(
        matches!(error, ProviderError::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
    );
    let mut spec = fixture.spec(&["cat"]);
    spec.cwd = fixture.root.join("missing-directory");
    assert!(exec::spawn(fixture.transport.clone(), &fixture.group, &spec).is_err());
}

#[test]
fn a_transport_that_never_acknowledges_startup_is_stopped() {
    let fixture = Fixture::new();
    let transport = Arc::new(LocalTransport {
        home: fixture.transport.home.clone(),
        tools: fixture.transport.tools.clone(),
        acknowledge: false,
    });
    let error = match exec::spawn(transport, &fixture.group, &fixture.spec(&["cat"])) {
        Err(error) => error,
        Ok(_) => panic!("unacknowledged startup was admitted"),
    };
    assert!(
        matches!(error, ProviderError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut)
    );
    assert!(branchyard_testkit::processes_in(fixture.root.path()).is_empty());
}

#[test]
fn teardown_reaches_descendants_after_the_started_program_exits() {
    let fixture = Fixture::new();
    let mut process = exec::spawn(
        fixture.transport.clone(),
        &fixture.group,
        &fixture.spec(&[
            "sh",
            "-c",
            "mkfifo held; cat held >/dev/null & echo $! > child.pid; exit 0",
        ]),
    )
    .unwrap();
    assert_eq!(process.wait().unwrap().code, Some(0));
    let pid: u32 = fs::read_to_string(fixture.root.join("work/child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(wait::alive(pid));
    let names = process.teardown();
    assert!(
        names.iter().any(|name| name == "cat" || name == "sh"),
        "{names:?}"
    );
    wait::gone(pid);
}

#[test]
fn stop_group_and_drop_end_running_execs() {
    let fixture = Fixture::new();
    let mut one = exec::spawn(
        fixture.transport.clone(),
        &fixture.group,
        &fixture.spec(&["cat"]),
    )
    .unwrap();
    let mut two = exec::spawn(
        fixture.transport.clone(),
        &fixture.group,
        &fixture.spec(&["cat"]),
    )
    .unwrap();
    exec::stop_group(fixture.transport.as_ref(), &fixture.group).unwrap();
    assert!(!one.wait().unwrap().success());
    assert!(!two.wait().unwrap().success());
    let process = exec::spawn(
        fixture.transport.clone(),
        &fixture.group,
        &fixture.spec(&["sh", "-c", "echo $$ > dropped.pid; exec cat"]),
    )
    .unwrap();
    let pid: u32 = wait::until("exec PID to be written", || {
        fs::read_to_string(fixture.root.join("work/dropped.pid"))
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
    });
    drop(process);
    wait::gone(pid);
}

#[test]
fn invalid_groups_arguments_environment_and_write_scopes_are_refused() {
    for invalid in ["", ".hidden", "two/names", "white space", "a;touch bad"] {
        assert!(ExecGroup::new(invalid).is_err());
    }
    let fixture = Fixture::new();
    for argv in [vec![], vec!["cat".into(), "bad\0argument".into()]] {
        let mut spec = fixture.spec(&["cat"]);
        spec.argv = argv;
        assert!(exec::spawn(fixture.transport.clone(), &fixture.group, &spec).is_err());
    }
    let mut spec = fixture.spec(&["cat"]);
    spec.env.insert("BAD VARIABLE".into(), "value".into());
    assert!(exec::spawn(fixture.transport.clone(), &fixture.group, &spec).is_err());
    spec.env.clear();
    spec.writable = Some(vec![fixture.root.join("work")]);
    assert!(matches!(
        exec::spawn(fixture.transport.clone(), &fixture.group, &spec),
        Err(ProviderError::Unsupported(_))
    ));
}
