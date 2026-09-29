pub const ENV_DISABLE_FILE_LOCK: &str = "LIMBO_DISABLE_FILE_LOCK";

/// Returns the parent directory of `path`. The parent of a bare filename is
/// the current directory.
#[cfg(unix)]
fn parent_dir(path: &str) -> &std::path::Path {
    std::path::Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
}

/// Fsync the parent directory of `path` so that its directory entry is
/// durable. Shared by the POSIX `IO::sync_parent_dir` impls.
#[cfg(unix)]
pub(crate) fn fsync_parent_dir(path: &str) -> crate::Result<()> {
    let dir = std::fs::File::open(parent_dir(path))
        .map_err(|e| crate::error::io_error(e, "open parent dir"))?;
    dir.sync_all()
        .map_err(|e| crate::error::io_error(e, "sync parent dir"))
}

#[cfg(test)]
pub mod tests {
    use crate::{Result, IO};
    use std::process::{Command, Stdio};
    use tempfile::NamedTempFile;

    fn run_test_parent_process<T: IO>(create_io: fn() -> Result<T>) {
        let temp_file: NamedTempFile = NamedTempFile::new().expect("Failed to create temp file");
        let path = temp_file.path().to_str().unwrap().to_string();

        // Parent process opens the file
        let io1 = create_io().expect("Failed to create IO");
        let _file1 = io1
            .open_file(&path, crate::io::OpenFlags::None, false)
            .expect("Failed to open file in parent process");

        let current_exe = std::env::current_exe().expect("Failed to get current executable path");

        // Spawn a child process and try to open the same file
        let child = Command::new(current_exe)
            .env("RUST_TEST_CHILD_PROCESS", "1")
            .env("RUST_TEST_FILE_PATH", &path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn child process");

        let output = child.wait_with_output().expect("Failed to wait on child");
        assert!(
            !output.status.success(),
            "Child process should have failed to open the file"
        );
    }

    fn run_test_child_process<T: IO>(create_io: fn() -> Result<T>) -> Result<()> {
        if std::env::var("RUST_TEST_CHILD_PROCESS").is_ok() {
            let path = std::env::var("RUST_TEST_FILE_PATH")?;
            let io = create_io()?;
            match io.open_file(&path, crate::io::OpenFlags::None, false) {
                Ok(_) => std::process::exit(0),
                Err(_) => std::process::exit(1),
            }
        }
        Ok(())
    }

    pub fn test_multiple_processes_cannot_open_file<T: IO>(create_io: fn() -> Result<T>) {
        run_test_child_process(create_io).unwrap();
        run_test_parent_process(create_io);
    }
}
