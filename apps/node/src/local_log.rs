//! macOS App 的本机日志。捕获现有 stdout/stderr，按行加时间，保留最近五个文件。

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 5 * 1024 * 1024;
const OLD_FILES: usize = 4;

pub fn install(data_dir: &Path) -> io::Result<()> {
    let dir = data_dir.join("logs");
    fs::create_dir_all(&dir)?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    rotate(&dir)?;
    let output = open_current(&dir)?;

    let mut fds = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let input = unsafe { File::from_raw_fd(fds[0]) };
    let pipe_writer = unsafe { File::from_raw_fd(fds[1]) };
    std::thread::Builder::new()
        .name("jaryn-local-log".into())
        .spawn(move || write_lines(input, output, dir))?;

    if unsafe { libc::dup2(pipe_writer.as_raw_fd(), libc::STDOUT_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::dup2(pipe_writer.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    drop(pipe_writer);
    println!("node: local log started");
    Ok(())
}

fn open_current(dir: &Path) -> io::Result<File> {
    let path = dir.join("client.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn rotate(dir: &Path) -> io::Result<()> {
    for n in (1..=OLD_FILES).rev() {
        let src = if n == 1 {
            dir.join("client.log")
        } else {
            dir.join(format!("client.log.{}", n - 1))
        };
        let dst = dir.join(format!("client.log.{n}"));
        if dst.exists() {
            fs::remove_file(&dst)?;
        }
        match fs::rename(src, dst) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write_lines(input: File, mut output: File, dir: PathBuf) {
    let mut reader = BufReader::new(input);
    let mut line = Vec::new();
    let mut bytes = output.metadata().map(|m| m.len()).unwrap_or(0);
    let mut writable = true;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if !writable {
            continue;
        }
        let text = String::from_utf8_lossy(&line);
        let entry = format!("[{}] {}", timestamp(), text);
        if bytes + entry.len() as u64 > MAX_BYTES && bytes > 0 {
            if rotate(&dir).is_ok() {
                if let Ok(file) = open_current(&dir) {
                    output = file;
                    bytes = 0;
                }
            }
        }
        if output.write_all(entry.as_bytes()).is_err() || output.flush().is_err() {
            writable = false;
            continue;
        }
        bytes += entry.len() as u64;
    }
}

fn timestamp() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs() as libc::time_t;
    let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&seconds, &mut local) }.is_null() {
        return elapsed.as_millis().to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        local.tm_year + 1900,
        local.tm_mon + 1,
        local.tm_mday,
        local.tm_hour,
        local.tm_min,
        local.tm_sec,
        elapsed.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_preserves_the_recent_files_in_order() {
        let dir = std::env::temp_dir().join(format!(
            "jaryn-log-test-{}-{}",
            std::process::id(),
            timestamp().replace([' ', ':', '.'], "-")
        ));
        fs::create_dir(&dir).unwrap();
        for n in 0..=OLD_FILES {
            let name = if n == 0 {
                "client.log".into()
            } else {
                format!("client.log.{n}")
            };
            fs::write(dir.join(name), n.to_string()).unwrap();
        }
        rotate(&dir).unwrap();
        assert!(!dir.join("client.log").exists());
        for n in 1..=OLD_FILES {
            assert_eq!(
                fs::read_to_string(dir.join(format!("client.log.{n}"))).unwrap(),
                (n - 1).to_string()
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
