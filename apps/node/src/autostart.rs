use std::path::{Path, PathBuf};

/// 开机自启默认关。只有在界面上被显式打开时才写系统项。
const FLAG: &str = "autostart";
const ARGS: &str = "launch.args";

fn flag_path(data_dir: &Path) -> PathBuf {
    data_dir.join(FLAG)
}

pub fn enabled(data_dir: &Path) -> bool {
    std::fs::read_to_string(flag_path(data_dir)).map(|s| s.trim() == "1").unwrap_or(false)
}

/// 记下这次启动用的参数。舰队令牌留在数据目录里，不写进 plist。
pub fn remember_args(data_dir: &Path, args: &[String]) -> Result<(), String> {
    let path = data_dir.join(ARGS);
    std::fs::write(&path, args.join("\n")).map_err(|e| e.to_string())?;
    restrict(&path);
    Ok(())
}

pub fn saved_args(data_dir: &Path) -> Vec<String> {
    std::fs::read_to_string(data_dir.join(ARGS))
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).map(|l| l.to_string()).collect())
        .unwrap_or_default()
}

pub fn set(data_dir: &Path, on: bool) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let label = format!("com.jaryn.remote.{}", sanitize(&exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()));
        let plist = dirs_launch_agents().join(format!("{label}.plist"));
        if on {
            if saved_args(data_dir).is_empty() {
                return Err("没有记下启动参数，不能设开机自启".into());
            }
            std::fs::create_dir_all(dirs_launch_agents()).map_err(|e| e.to_string())?;
            let body = plist_body(&label, &exe, data_dir);
            std::fs::write(&plist, body).map_err(|e| e.to_string())?;
            let _ = std::process::Command::new("launchctl")
                .args(["load", "-w"])
                .arg(&plist)
                .status();
        } else {
            if plist.exists() {
                let _ = std::process::Command::new("launchctl")
                    .args(["unload", "-w"])
                    .arg(&plist)
                    .status();
                let _ = std::fs::remove_file(&plist);
            }
        }
    }
    #[cfg(windows)]
    {
        if on && saved_args(data_dir).is_empty() {
            return Err("没有记下启动参数，不能设开机自启".into());
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let args = vec!["--auto-start-from".into(), data_dir.to_string_lossy().into_owned()];
        jr_host_win::autostart::set(&exe, &args, on)?;
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (data_dir, on);
        return Err("这个系统还没有自启适配器".into());
    }
    // 记下我们上次设成什么：注册表和 plist 都可能被用户手动改过，
    // 界面要显示的是「我们设的状态」，不是系统当前的样子。
    let path = flag_path(data_dir);
    std::fs::write(&path, if on { "1" } else { "0" }).map_err(|e| e.to_string())?;
    restrict(&path);
    Ok(())
}

#[cfg(target_os = "macos")]
fn sanitize(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect()
}

#[cfg(target_os = "macos")]
fn dirs_launch_agents() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Library/LaunchAgents")
}

#[cfg(target_os = "macos")]
fn plist_body(label: &str, exe: &Path, data_dir: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>--auto-start-from</string>
    <string>{}</string>
  </array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
        exe.display(),
        data_dir.display()
    )
}

fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}
