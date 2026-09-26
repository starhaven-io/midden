use std::ffi::{OsStr, OsString};
use std::path::Path;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Whether a `claude` process is currently running.
pub fn claude_is_running() -> bool {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    let me = std::process::id();
    has_claude_process(
        sys.processes()
            .values()
            .map(|process| (process.pid().as_u32(), process.name(), process.cmd())),
        me,
    )
}

fn has_claude_process<'a>(
    processes: impl IntoIterator<Item = (u32, &'a OsStr, &'a [OsString])>,
    me: u32,
) -> bool {
    processes
        .into_iter()
        .any(|(pid, name, cmd)| pid != me && is_claude(name, cmd))
}

/// A native install runs as `claude`. An npm install runs under `node` or
/// `bun` with the `claude` script as its argument, and macOS keeps the
/// interpreter's name even when Claude Code sets its process title. Only that
/// script argument is inspected: editors and shells often mention "claude" in
/// their arguments, and they must not trip the safety gate.
fn is_claude(name: &OsStr, cmd: &[OsString]) -> bool {
    let claude_name = |value: &OsStr| {
        value
            .to_str()
            .is_some_and(|v| v == "claude" || v == "claude-code")
    };
    if claude_name(name) {
        return true;
    }
    if !name
        .to_str()
        .is_some_and(|name| name == "node" || name == "bun")
    {
        return false;
    }
    cmd.iter()
        .skip(1)
        .find(|argument| !argument.to_string_lossy().starts_with('-'))
        .is_some_and(|script| {
            Path::new(script).file_name().is_some_and(claude_name)
                || script
                    .to_string_lossy()
                    .contains("/@anthropic-ai/claude-code/")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(arguments: &[&str]) -> Vec<OsString> {
        arguments.iter().map(OsString::from).collect()
    }

    #[test]
    fn process_matching_is_exact_and_ignores_self() {
        let me = 7;
        let none = cmd(&[]);
        assert!(!has_claude_process([], me));
        assert!(!has_claude_process(
            [(me, OsStr::new("claude"), none.as_slice())],
            me
        ));
        assert!(!has_claude_process(
            [(8, OsStr::new("claude-editor"), none.as_slice())],
            me
        ));
        assert!(has_claude_process(
            [(8, OsStr::new("claude"), none.as_slice())],
            me
        ));
        assert!(has_claude_process(
            [(8, OsStr::new("claude-code"), none.as_slice())],
            me
        ));
    }

    #[test]
    fn node_hosted_claude_is_matched_by_its_script() {
        assert!(is_claude(
            OsStr::new("node"),
            &cmd(&["node", "/opt/homebrew/bin/claude"])
        ));
        assert!(is_claude(
            OsStr::new("node"),
            &cmd(&[
                "node",
                "--no-warnings",
                "/x/node_modules/@anthropic-ai/claude-code/cli.js"
            ])
        ));
        assert!(is_claude(
            OsStr::new("bun"),
            &cmd(&["bun", "/x/bin/claude"])
        ));
        assert!(!is_claude(OsStr::new("node"), &cmd(&["node", "server.js"])));
        assert!(!is_claude(
            OsStr::new("node"),
            &cmd(&["node", "/usr/local/bin/claude-editor"])
        ));
        assert!(!is_claude(
            OsStr::new("vim"),
            &cmd(&["vim", "/x/node_modules/@anthropic-ai/claude-code/cli.js"])
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_process_names_do_not_panic_or_match() {
        use std::os::unix::ffi::OsStrExt;

        assert!(!is_claude(OsStr::from_bytes(b"claude\xff"), &cmd(&[])));
    }
}
