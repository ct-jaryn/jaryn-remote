//! Windows 上的开机自启。
//!
//! 写的是当前用户的 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，
//! 也就是「用户的启动项」。不装服务，不写 HKLM，不需要管理员，
//! 所以不会出现「登录前就在采集」这种情况——那本来就是非目标。
//!
//! 默认关。只有用户在界面上显式打开时才会写这一项。

use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "JarynRemote";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 打开启动项所在的键。调用方负责关掉。
fn open_key(write: bool) -> Result<HKEY, String> {
    let mut key = HKEY::default();
    let access = if write { KEY_SET_VALUE } else { KEY_QUERY_VALUE };
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(RUN_KEY).as_ptr()),
            Some(0),
            access,
            &mut key,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(format!("打不开启动项注册表键: {rc:?}"));
    }
    Ok(key)
}

/// 打开或关闭开机自启。参数里带上舰队令牌等启动参数，否则开机起来是个空壳。
pub fn set(exe: &Path, args: &[String], on: bool) -> Result<(), String> {
    let key = open_key(true)?;
    let result = (|| -> Result<(), String> {
        if on {
            let mut cmd = String::from("\"");
            cmd.push_str(&exe.to_string_lossy());
            cmd.push('"');
            for a in args {
                cmd.push(' ');
                cmd.push_str(&quote(a));
            }
            let value = wide(&cmd);
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    value.as_ptr() as *const u8,
                    value.len() * std::mem::size_of::<u16>(),
                )
            };
            let rc = unsafe {
                RegSetValueExW(
                    key,
                    PCWSTR(wide(VALUE_NAME).as_ptr()),
                    Some(0),
                    REG_SZ,
                    Some(bytes),
                )
            };
            if rc != ERROR_SUCCESS {
                return Err(format!("写启动项失败: {rc:?}"));
            }
            Ok(())
        } else {
            let rc = unsafe { RegDeleteValueW(key, PCWSTR(wide(VALUE_NAME).as_ptr())) };
            if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                Err(format!("删启动项失败: {rc:?}"))
            }
        }
    })();
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

/// 现在开着没有。注册表里没有这一项就是关。
pub fn enabled() -> bool {
    let Ok(key) = open_key(false) else {
        return false;
    };
    let mut size = 0u32;
    let rc = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(wide(VALUE_NAME).as_ptr()),
            None,
            None,
            None,
            Some(&mut size),
        )
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    rc == ERROR_SUCCESS
}

/// 参数里有空格或引号就包起来。不做转义，Windows 的命令行本来就不支持转义。
fn quote(arg: &str) -> String {
    if arg.contains(' ') || arg.contains('"') {
        format!("\"{}\"", arg.replace('"', ""))
    } else {
        arg.to_string()
    }
}
