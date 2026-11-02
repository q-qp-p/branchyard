//! [`RecipeProvider`]: a [`SandboxProvider`] whose sandboxes are machines a
//! recipe's scripts create, and whose execs run there over the result's
//! transport.
//!
//! - `ensure` runs `create` (once per name) and keeps its result; a spec
//!   with mounts, an image or resource limits is refused, since the
//!   machine is whatever the script makes and cannot see this host's
//!   directories.
//! - `exec` runs the argument vector, without a shell of its own, in the
//!   result's `projectRoot` with the result's `env` and the spec's, through
//!   a small `sh` wrapper on the machine that first makes it a process
//!   group leader (with `setsid` when the transport did not), records the
//!   group in `~/.branchyard/recipe-exec/<sandbox>/`, and reports a
//!   program it cannot find before anything starts, so that `exec` fails
//!   with an I/O error as the contract asks.
//! - `kill`, `teardown` and `stop` signal that recorded group on the
//!   machine, since closing an ssh connection leaves remote processes
//!   running; `teardown` names what it found.
//! - `pause` runs `suspend` and `resume` runs `resume`, declared only when
//!   the recipe has both; `destroy` runs `destroy` and forgets the machine.
//!
//! With a state directory, each machine's result is kept in a file, so a
//! later process can exec in, pause, resume or destroy it by name.

use branchyard_support::LockExt as _;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use branchyard_sandbox::{
    Capabilities, ExecSpec, Operation, Process, ProviderError, SandboxInfo, SandboxProvider,
    SandboxSpec, SandboxState,
};
use serde::{Deserialize, Serialize};

use crate::transport::Transport;
use crate::{parse_result, run, Mode, Payload, Recipe, RecipeResult};
use branchyard_sandbox::exec::{self, ExecGroup};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Held {
    Running,
    Paused,
    Stopped,
}

impl Held {
    fn state(self) -> SandboxState {
        match self {
            Held::Running => SandboxState::Running,
            Held::Paused => SandboxState::Paused,
            Held::Stopped => SandboxState::Stopped,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Machine {
    result: RecipeResult,
    held: Held,
}

/// See the module documentation.
pub struct RecipeProvider {
    recipe: Recipe,
    ssh: String,
    state_dir: Option<PathBuf>,
    machines: Mutex<BTreeMap<String, Machine>>,
}

impl RecipeProvider {
    /// A provider over `recipe`, with `ssh` as the ssh program.
    pub fn new(recipe: Recipe, ssh: impl Into<String>) -> RecipeProvider {
        RecipeProvider {
            recipe,
            ssh: ssh.into(),
            state_dir: None,
            machines: Mutex::new(BTreeMap::new()),
        }
    }

    /// Keep each machine's result in `dir`, for later processes.
    pub fn with_state_dir(mut self, dir: impl Into<PathBuf>) -> RecipeProvider {
        self.state_dir = Some(dir.into());
        self
    }

    pub fn recipe(&self) -> &Recipe {
        &self.recipe
    }

    /// What `create` (or the last `resume`) printed for `name`.
    pub fn result(&self, name: &str) -> Option<RecipeResult> {
        self.machine(name).map(|m| m.result)
    }

    /// How execs reach `name`.
    pub fn transport(&self, name: &str) -> Option<Transport> {
        self.machine(name)
            .map(|m| Transport::new(&m.result.connection, &self.ssh))
    }

    fn file(&self, name: &str) -> Option<PathBuf> {
        self.state_dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.json", key(name))))
    }

    fn machine(&self, name: &str) -> Option<Machine> {
        let mut machines = self.machines.lock_recovering("machines");
        if let Some(machine) = machines.get(name) {
            return Some(machine.clone());
        }
        let text = std::fs::read_to_string(self.file(name)?).ok()?;
        let machine: Machine = serde_json::from_str(&text).ok()?;
        machines.insert(name.to_owned(), machine.clone());
        Some(machine)
    }

    fn record(&self, name: &str, machine: Option<Machine>) -> Result<(), ProviderError> {
        let mut machines = self.machines.lock_recovering("machines");
        match &machine {
            Some(machine) => {
                machines.insert(name.to_owned(), machine.clone());
            }
            None => {
                machines.remove(name);
            }
        }
        let Some(file) = self.file(name) else {
            return Ok(());
        };
        match machine {
            Some(machine) => {
                use std::os::unix::fs::OpenOptionsExt;
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                let text = serde_json::to_string_pretty(&machine).map_err(io::Error::other)?;
                let temporary = file.with_extension(format!("json.{}", std::process::id()));
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&temporary)?
                    .write_all(text.as_bytes())?;
                std::fs::rename(&temporary, &file)?;
            }
            None => {
                branchyard_support::cleanup_file(&file);
            }
        }
        Ok(())
    }

    /// Run a lifecycle script with Orca's payload on stdin.
    fn lifecycle(
        &self,
        name: &str,
        command: &str,
        mode: Mode,
        result: &RecipeResult,
    ) -> Result<String, ProviderError> {
        let payload = Payload {
            schema_version: 1,
            mode,
            recipe: &self.recipe.name,
            instance: name,
            recipe_result: result,
        };
        let mut input = serde_json::to_vec(&payload).map_err(io::Error::other)?;
        input.push(b'\n');
        let ran = run(&self.recipe, command, mode, name, Some(&input))?;
        match ran.success() {
            true => Ok(ran.stdout),
            false => Err(ProviderError::Runtime(ran.failure(&self.recipe.name, mode))),
        }
    }

    fn stop_execs(&self, name: &str) -> io::Result<()> {
        let Some(transport) = self.transport(name) else {
            return Ok(());
        };
        let group = ExecGroup::new(key(name)).map_err(io::Error::from)?;
        exec::stop_group(&transport, &group)
    }
}

/// A sandbox name as a file name.
fn key(name: &str) -> String {
    name.chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                true => c,
                false => '_',
            },
        )
        .collect::<String>()
        .trim_start_matches('.')
        .to_owned()
}

impl SandboxProvider for RecipeProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            exec: true,
            pause: self.recipe.can_pause(),
            // The machine is the recipe's; nothing here confines its network
            // (docs/egress.md).
            egress: false,
            ..Capabilities::default()
        }
    }

    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
        spec.validate()?;
        if !spec.mounts.is_empty() {
            return Err(ProviderError::Invalid(format!(
                "recipe {}'s machine cannot mount this host's directories ({})",
                self.recipe.name,
                spec.mounts[0].host.display()
            )));
        }
        if spec.image.is_some() {
            return Err(ProviderError::Invalid(format!(
                "recipe {}'s machine is what its create script makes; it takes no image",
                self.recipe.name
            )));
        }
        if !spec.resources.is_unlimited() {
            return Err(ProviderError::Invalid(format!(
                "recipe {} sets its machine's resources itself; the spec cannot",
                self.recipe.name
            )));
        }
        if let Some(mut machine) = self.machine(&spec.name) {
            if machine.held == Held::Stopped {
                machine.held = Held::Running;
                self.record(&spec.name, Some(machine.clone()))?;
            }
            return Ok(SandboxInfo {
                name: spec.name.clone(),
                state: machine.held.state(),
            });
        }
        let ran = run(
            &self.recipe,
            &self.recipe.create,
            Mode::Create,
            &spec.name,
            None,
        )?;
        if !ran.success() {
            return Err(ProviderError::Runtime(
                ran.failure(&self.recipe.name, Mode::Create),
            ));
        }
        let result = parse_result(&ran.stdout).map_err(|e| {
            ProviderError::Runtime(format!("recipe {}: create: {e}", self.recipe.name))
        })?;
        self.record(
            &spec.name,
            Some(Machine {
                result,
                held: Held::Running,
            }),
        )?;
        Ok(SandboxInfo {
            name: spec.name.clone(),
            state: SandboxState::Running,
        })
    }

    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        Ok(self.machine(name).map(|m| SandboxInfo {
            name: name.to_owned(),
            state: m.held.state(),
        }))
    }

    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        // A recipe's machine has no way to hold a process to a write scope.
        spec.refuse_write_scope()?;
        let machine = self
            .machine(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        match machine.held {
            Held::Running => {}
            Held::Paused => {
                return Err(ProviderError::Invalid(format!(
                    "{name} is paused; resume it first"
                )))
            }
            Held::Stopped => {
                return Err(ProviderError::Invalid(format!(
                    "{name} is stopped; ensure it again first"
                )))
            }
        }
        let mut spec = spec.clone();
        if spec.cwd.as_os_str().is_empty() {
            spec.cwd = machine.result.connection.project_root().into();
        }
        let mut env: BTreeMap<OsString, OsString> = machine
            .result
            .env
            .iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect();
        env.extend(spec.env);
        spec.env = env;
        let transport = Arc::new(Transport::new(&machine.result.connection, &self.ssh));
        exec::spawn(transport, &ExecGroup::new(key(name))?, &spec)
    }

    fn stop(&self, name: &str) -> Result<(), ProviderError> {
        let mut machine = self
            .machine(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        if machine.held == Held::Running {
            self.stop_execs(name)?;
            machine.held = Held::Stopped;
            self.record(name, Some(machine))?;
        }
        Ok(())
    }

    fn destroy(&self, name: &str) -> Result<(), ProviderError> {
        let Some(machine) = self.machine(name) else {
            return Ok(());
        };
        if machine.held == Held::Running {
            // Best effort: the machine is going away regardless.
            branchyard_support::best_effort(
                "send the command to the remote provider",
                self.stop_execs(name),
            );
        }
        if let Some(destroy) = &self.recipe.destroy {
            self.lifecycle(name, destroy, Mode::Destroy, &machine.result)?;
        }
        self.record(name, None)
    }

    fn pause(&self, name: &str) -> Result<(), ProviderError> {
        let (Some(suspend), true) = (&self.recipe.suspend, self.recipe.can_pause()) else {
            return Err(ProviderError::unsupported(Operation::Pause));
        };
        let mut machine = self
            .machine(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        if machine.held == Held::Paused {
            return Ok(());
        }
        self.lifecycle(name, suspend, Mode::Suspend, &machine.result)?;
        machine.held = Held::Paused;
        self.record(name, Some(machine))
    }

    fn resume(&self, name: &str) -> Result<SandboxInfo, ProviderError> {
        let (Some(resume), true) = (&self.recipe.resume, self.recipe.can_pause()) else {
            return Err(ProviderError::unsupported(Operation::Resume));
        };
        let mut machine = self
            .machine(name)
            .ok_or_else(|| ProviderError::NotFound(name.to_owned()))?;
        if machine.held != Held::Running {
            let stdout = self.lifecycle(name, resume, Mode::Resume, &machine.result)?;
            machine.result = parse_result(&stdout).map_err(|e| {
                ProviderError::Runtime(format!("recipe {}: resume: {e}", self.recipe.name))
            })?;
            machine.held = Held::Running;
            self.record(name, Some(machine))?;
        }
        Ok(SandboxInfo {
            name: name.to_owned(),
            state: SandboxState::Running,
        })
    }
}
