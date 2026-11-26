//! Windows 上的伪终端，也就是 ConPTY。
//!
//! 没有它，Windows 上的终端只能是一根管道：`isatty` 全是假，方向键和行编辑
//! 都不工作，窗口尺寸也没地方生效。这条路上做的事和别的平台一样——
//! 读写都走同一块伪终端，尺寸变化是真的送进去。
//!
//! **状态：已在 Windows (`x86_64-pc-windows-msvc`) 通过集成测试验证启动、交互回显、
//! `ResizePseudoConsole` 窗口尺寸同步与关闭回收。**

use std::io::{self, Read, Write};

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
    TerminateProcess, UpdateProcThreadAttribute, EXTENDED_STARTUPINFO_PRESENT,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
    STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

pub struct ConPty {
    pcon: HPCON,
    /// 写进这里就是喂给 shell 的输入。
    input: HANDLE,
    /// shell 的输出从这儿读；交给 HandleReader 后由读线程退出时关闭。
    output: Option<HANDLE>,
    process: HANDLE,
    closed: bool,
}

// 句柄是内核对象，跨线程用没问题；这里手动声明，因为默认不实现。
unsafe impl Send for ConPty {}

impl ConPty {
    /// 起一个 shell。`command` 为空就用 %COMSPEC%。
    pub fn spawn(command: &str, cols: u16, rows: u16) -> io::Result<Self> {
        unsafe {
            let mut input_read = HANDLE::default();
            let mut input_write = HANDLE::default();
            let mut output_read = HANDLE::default();
            let mut output_write = HANDLE::default();
            CreatePipe(&mut input_read, &mut input_write, None, 0).map_err(io_err)?;
            if let Err(e) = CreatePipe(&mut output_read, &mut output_write, None, 0) {
                let _ = CloseHandle(input_read);
                let _ = CloseHandle(input_write);
                return Err(io_err(e));
            }

            let size = COORD {
                X: cols.max(1) as i16,
                Y: rows.max(1) as i16,
            };
            // ConPTY 拿走的是「读端」和「写端」：它从 input_read 读输入，
            // 往 output_write 写输出。剩下的两头才是我们用的。
            let pcon = match CreatePseudoConsole(size, input_read, output_write, 0) {
                Ok(p) => p,
                Err(e) => {
                    for h in [input_read, input_write, output_read, output_write] {
                        let _ = CloseHandle(h);
                    }
                    return Err(io_err(e));
                }
            };
            let _ = CloseHandle(input_read);
            let _ = CloseHandle(output_write);

            let process = match launch(command, pcon) {
                Ok(h) => h,
                Err(e) => {
                    ClosePseudoConsole(pcon);
                    let _ = CloseHandle(input_write);
                    let _ = CloseHandle(output_read);
                    return Err(e);
                }
            };
            Ok(ConPty {
                pcon,
                input: input_write,
                output: Some(output_read),
                process,
                closed: false,
            })
        }
    }

    /// 改窗口尺寸。内核会给 shell 发窗口变化通知，和 unix 上一样。
    pub fn resize(&self, cols: u16, rows: u16) {
        let size = COORD {
            X: cols.max(1) as i16,
            Y: rows.max(1) as i16,
        };
        unsafe {
            let _ = ResizePseudoConsole(self.pcon, size);
        }
    }

    /// 读端。拿走独占读取器，读线程退出时再关句柄，避免在同步 ReadFile 阻塞期间跨线程关句柄死锁。
    pub fn reader(&mut self) -> io::Result<Box<dyn Read + Send>> {
        let handle = self
            .output
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "伪终端读端已被取走"))?;
        Ok(Box::new(HandleReader { handle }))
    }

    /// 把输入写进伪终端。
    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "伪终端已关闭"));
        }
        let mut written = 0u32;
        let total = bytes.len() as u32;
        unsafe {
            let ok = windows::Win32::Storage::FileSystem::WriteFile(
                self.input,
                Some(bytes),
                Some(&mut written),
                None,
            );
            if ok.is_err() || written != total {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "写不进伪终端"));
            }
        }
        Ok(())
    }

    /// 结束这个 shell。必须先关伪终端让读线程收到 EOF，并且保证幂等避免 Drop 二次释放。
    pub fn kill(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        unsafe {
            let _ = TerminateProcess(self.process, 1);
            let _ = CloseHandle(self.input);
            ClosePseudoConsole(self.pcon);
            let _ = CloseHandle(self.process);
            if let Some(output) = self.output.take() {
                let _ = CloseHandle(output);
            }
        }
    }
}

impl Drop for ConPty {
    fn drop(&mut self) {
        self.kill();
    }
}

struct HandleReader {
    handle: HANDLE,
}

// 句柄是内核对象，跨线程读没问题。
unsafe impl Send for HandleReader {}

impl Read for HandleReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut got = 0u32;
        unsafe {
            let ok = windows::Win32::Storage::FileSystem::ReadFile(
                self.handle,
                Some(buf),
                Some(&mut got),
                None,
            );
            match ok {
                Ok(()) => Ok(got as usize),
                Err(_) => Err(io::Error::new(io::ErrorKind::BrokenPipe, "伪终端读不出来")),
            }
        }
    }
}

impl Drop for HandleReader {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

impl Write for ConPty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        ConPty::write(self, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn launch(command: &str, pcon: HPCON) -> io::Result<HANDLE> {
    unsafe {
        let mut size = 0usize;
        let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
        let mut storage = vec![0u8; size];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr() as *mut _);
        InitializeProcThreadAttributeList(Some(list), 1, None, &mut size).map_err(io_err)?;
        // 把伪终端挂到子进程上。少了这一步，子进程看不到这块伪终端。
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            Some(pcon.0 as *const _),
            std::mem::size_of::<HPCON>(),
            None,
            None,
        )
        .map_err(io_err)?;

        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        // 显式置空标准句柄标志，防止父进程在重定向管道下启动时子进程绕过 ConPTY 继承父进程管道。
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.lpAttributeList = list;
        let mut pi = PROCESS_INFORMATION::default();

        let mut cmdline: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        let created = CreateProcessW(
            None,
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            EXTENDED_STARTUPINFO_PRESENT,
            None,
            None,
            &si.StartupInfo,
            &mut pi,
        );
        DeleteProcThreadAttributeList(list);
        created.map_err(io_err)?;
        let _ = CloseHandle(pi.hThread);
        Ok(pi.hProcess)
    }
}

fn io_err(e: windows::core::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

/// %COMSPEC% 是 Windows 上的 $SHELL。
pub fn default_shell() -> String {
    std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
}
