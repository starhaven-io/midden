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
    sys.processes().values().any(|p| {
        if p.pid().as_u32() == me {
            return false;
        }
        let Some(name) = p.name().to_str() else {
            return false;
        };
        name == "claude" || name == "claude-code"
    })
}
