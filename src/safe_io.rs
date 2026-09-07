use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

pub const MAX_CLAUDE_JSON_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_CONFIG_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_INSTRUCTION_BYTES: usize = 4 * 1024 * 1024;

/// Read a regular file without allowing devices or FIFOs to block the process,
/// and without allocating beyond the caller's file-class budget. Symlinks to
/// regular files remain supported because both providers support them in
/// user-controlled configuration trees.
pub fn read_to_string(path: &Path, max_bytes: usize) -> io::Result<String> {
    let file = open_regular(path, true)?;
    let metadata = file.metadata()?;
    if metadata.len() > max_bytes as u64 {
        return Err(too_large(path, max_bytes));
    }

    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(max_bytes));
    file.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(too_large(path, max_bytes));
    }
    String::from_utf8(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not valid UTF-8: {error}", path.display()),
        )
    })
}

fn too_large(path: &Path, max_bytes: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!(
            "{} exceeds the {} byte read limit",
            path.display(),
            max_bytes
        ),
    )
}

/// Open and validate a regular file. `follow_symlinks` is appropriate for
/// provider-controlled configuration trees; generated transcript evidence
/// passes false so a substituted link cannot redirect the read.
pub fn open_regular(path: &Path, follow_symlinks: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut flags = libc::O_NONBLOCK | libc::O_CLOEXEC;
        if !follow_symlinks {
            flags |= libc::O_NOFOLLOW;
        }
        options.custom_flags(flags);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a regular file: {}", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_exact_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input");
        std::fs::write(&path, "1234").unwrap();
        assert_eq!(read_to_string(&path, 4).unwrap(), "1234");
        let error = read_to_string(&path, 3).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
    }

    #[test]
    fn rejects_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input");
        std::fs::write(&path, [0xff]).unwrap();
        assert_eq!(
            read_to_string(&path, 4).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn supports_symlinks_to_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        std::fs::write(&target, "ok").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(read_to_string(&link, 2).unwrap(), "ok");
        assert!(open_regular(&link, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    #[expect(
        unsafe_code,
        reason = "creating a FIFO fixture requires the platform mkfifo call"
    )]
    fn rejects_a_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe");
        let raw = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: raw is a NUL-terminated path owned for the duration of the call.
        let status = unsafe { libc::mkfifo(raw.as_ptr(), 0o600) };
        assert_eq!(status, 0);
        assert_eq!(
            read_to_string(&path, 16).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
