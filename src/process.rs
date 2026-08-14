use std::ffi::OsStr;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

/// Whether a `claude` process is currently running.
///
/// We deliberately match only the canonical binary name. Editors and shells
/// commonly have "claude" in window titles or arguments, and we don't want
/// those to trip the safety gate.
pub fn claude_is_running() -> bool {
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let me = std::process::id();
    has_claude_process(
        sys.processes()
            .values()
            .map(|process| (process.pid().as_u32(), process.name())),
        me,
    )
}

fn has_claude_process<'a>(processes: impl IntoIterator<Item = (u32, &'a OsStr)>, me: u32) -> bool {
    processes.into_iter().any(|(pid, name)| {
        pid != me
            && name
                .to_str()
                .is_some_and(|name| name == "claude" || name == "claude-code")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_matching_is_exact_and_ignores_self() {
        let me = 7;
        assert!(!has_claude_process([], me));
        assert!(!has_claude_process([(me, OsStr::new("claude"))], me));
        assert!(!has_claude_process([(8, OsStr::new("claude-editor"))], me));
        assert!(has_claude_process([(8, OsStr::new("claude"))], me));
        assert!(has_claude_process([(8, OsStr::new("claude-code"))], me));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_process_names_do_not_panic_or_match() {
        use std::os::unix::ffi::OsStrExt;

        assert!(!has_claude_process(
            [(8, OsStr::from_bytes(b"claude\xff"))],
            7
        ));
    }
}
