//! 伪终端。
//!
//! 用管道跑 shell 只能看回显，`isatty` 全是假，方向键和行编辑都不工作，
//! 窗口尺寸也没地方生效过。这里用 pty，读写都走同一个主设备。
//!
//! unix 用 openpty，Windows 走宿主适配器里的 ConPTY。上层只认
//! spawn/write/resize/reader/kill 这五个动作，不认系统。

#[cfg(unix)]
pub use unix::*;

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::unix::io::{FromRawFd, RawFd};
    #[allow(unused_imports)]
    use std::os::unix::io::AsRawFd;
    use std::process::{Child, Command, Stdio};

    pub struct Pty {
        /// 主设备。写进去就是喂给 shell 的输入。
        pub master: std::fs::File,
        pub child: Child,
        fd: RawFd,
    }

    impl Pty {
        pub fn resize(&self, cols: u16, rows: u16) {
            resize(self.fd, cols, rows);
        }

        pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            use std::io::Write;
            self.master.write_all(bytes)?;
            self.master.flush()
        }

        /// 结束这个 shell，并等它退干净。等不到就不等，别把会话卡住。
        pub fn kill(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }

        /// 读端。复制一份主设备，写端继续留在 Pty 里。
        pub fn reader(&mut self) -> io::Result<Option<Box<dyn std::io::Read + Send>>> {
            let raw = self.fd;
            let copy = unsafe { libc::dup(raw) };
            if copy < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Some(Box::new(unsafe { std::fs::File::from_raw_fd(copy) })))
        }
    }

    pub fn resize(fd: RawFd, cols: u16, rows: u16) {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // 内核会在改了窗口大小之后自己给前台进程组发 SIGWINCH。
        unsafe {
            libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
        }
    }

    pub fn spawn(term: &str, cols: u16, rows: u16) -> io::Result<Pty> {
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        let mut ws = libc::winsize {
            ws_row: rows.max(1),
            ws_col: cols.max(1),
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let dup = |fd: RawFd| -> io::Result<Stdio> {
            let copy = unsafe { libc::dup(fd) };
            if copy < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { Stdio::from_raw_fd(copy) })
        };
        let stdin = dup(slave)?;
        let stdout = dup(slave)?;
        let stderr = dup(slave)?;
        let mut cmd = Command::new(shell_path());
        cmd.env("TERM", if term.is_empty() { "xterm-256color" } else { term })
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr);
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(move || {
                // 自成会话，并把这块 pty 认成控制终端。少了这步，
                // 窗口尺寸变化不会变成信号送到 shell 手上。
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn();
        unsafe {
            libc::close(slave);
        }
        let child = match child {
            Ok(c) => c,
            Err(e) => {
                unsafe {
                    libc::close(master);
                }
                return Err(e);
            }
        };
        Ok(Pty {
            master: unsafe { std::fs::File::from_raw_fd(master) },
            child,
            fd: master,
        })
    }

    fn shell_path() -> String {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

#[cfg(windows)]
mod win {
    use std::io;

    /// Windows 上的伪终端就是宿主适配器里的 ConPTY。
    pub struct Pty {
        inner: jr_host_win::conpty::ConPty,
    }

    impl Pty {
        pub fn resize(&self, cols: u16, rows: u16) {
            self.inner.resize(cols, rows);
        }

        pub fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.inner.write(bytes)
        }

        pub fn kill(&mut self) {
            self.inner.kill();
        }

        pub fn reader(&mut self) -> io::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(Some(self.inner.reader()?))
        }
    }

    pub fn spawn(_term: &str, cols: u16, rows: u16) -> io::Result<Pty> {
        // 这里的 `term` 是终端类型（如 xterm-256color），不是可执行文件。
        // ConPTY 要一条真的命令行，Windows 上统一用默认 shell。
        let shell = jr_host_win::conpty::default_shell();
        let inner = jr_host_win::conpty::ConPty::spawn(&shell, cols, rows)?;
        Ok(Pty { inner })
    }
}

#[cfg(not(any(unix, windows)))]
mod fallback {
    use std::io;

    pub struct Pty;

    impl Pty {
        pub fn resize(&self, _cols: u16, _rows: u16) {}

        pub fn write(&mut self, _bytes: &[u8]) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::Unsupported, "这个系统还没有伪终端适配器"))
        }

        pub fn kill(&mut self) {}

        pub fn reader(&mut self) -> io::Result<Option<Box<dyn std::io::Read + Send>>> {
            Ok(None)
        }
    }

    pub fn spawn(_term: &str, _cols: u16, _rows: u16) -> io::Result<Pty> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "这个系统还没有伪终端适配器",
        ))
    }
}

#[cfg(windows)]
pub use win::*;

#[cfg(not(any(unix, windows)))]
pub use fallback::*;
