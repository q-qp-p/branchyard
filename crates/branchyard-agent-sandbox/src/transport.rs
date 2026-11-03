use std::process::Command;

use branchyard_sandbox::exec::ExecTransport;

use crate::control::Control;

// All user scripts remain positional parameters. The first instruction reads
// the actual Pod UID from a kernel-mounted, read-only DownwardAPI volume. Even
// if kubectl's name lookup reaches a replacement Pod, no payload runs there.
const GUARDED: &str = r#"actual=$(cat /var/run/branchyard-identity/pod-uid) || exit 125
[ "$actual" = "$1" ] || { printf '%s\n' 'branchyard: replacement Pod refused' >&2; exit 125; }
shift
script=$1
shift
exec sh -c "$script" sh "$@""#;

pub(crate) struct Transport {
    pub control: Control,
    pub pod_name: String,
    pub pod_uid: String,
}

impl ExecTransport for Transport {
    fn command(&self, script: &str, args: &[&str]) -> Command {
        let kubectl = self.control.namespaced();
        let mut command = Command::new(&self.control.python);
        command.args(["-c", include_str!("kubectl_exec.py")]);
        command.arg(kubectl.get_program()).args(kubectl.get_args());
        command.args([
            "exec",
            "-i",
            &self.pod_name,
            "-c",
            "runtime",
            "--",
            "setpriv",
            "--no-new-privs",
            "--",
            "sh",
            "-c",
            GUARDED,
            "sh",
            &self.pod_uid,
            script,
        ]);
        command.args(args);
        command
    }

    fn describe(&self) -> String {
        format!(
            "kubectl exec {}/{} (Pod UID {})",
            self.control.config.namespace, self.pod_name, self.pod_uid
        )
    }

    fn diagnostic(&self, bytes: &[u8]) -> Vec<u8> {
        if bytes.is_empty()
            || matches!(
                bytes,
                b"guest program was not found" | b"guest exec directory could not be entered"
            )
        {
            bytes.to_vec()
        } else {
            b"kubectl transport diagnostic omitted\n".to_vec()
        }
    }
}
