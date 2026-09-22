use std::path::{Path, PathBuf};

pub fn app_dir(base: &Path) -> PathBuf {
    base.join("claude-hud")
}

pub fn sessions_dir(base: &Path) -> PathBuf {
    app_dir(base).join("sessions")
}

pub fn config_path(base: &Path) -> PathBuf {
    app_dir(base).join("config.json")
}

/// `%APPDATA%` 的 **base**（**不是** app 目录）。
/// `sessions_dir` / `config_path` 收的是 base —— 把它们喂给 `app_dir` 的结果会得到
/// `…\claude-hud\claude-hud\…`。这个命名陷阱是 Task 1 评审记下的。
///
/// （Task 1 另有一个返回 app 目录的 `real_app_dir()`，随 Ruling #7 的到期清理在
/// Task 12 删除：自 T11 改用 `real_config_path()` 之后它就没有调用者了。）
fn real_base() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 集成测试用：允许用环境变量把状态目录指到临时目录。
/// 生产路径不受影响——不设这个变量时行为与之前完全一致。
pub fn real_sessions_dir() -> PathBuf {
    if let Some(over) = std::env::var_os("CLAUDE_HUD_SESSIONS_DIR") {
        return PathBuf::from(over);
    }
    sessions_dir(&real_base())
}

/// Claude Code 的**转录根目录**：`~/.claude/projects/`。
///
/// token 账本要在这里走一遍（每份转录 = 一条会话）。与 `real_sessions_dir()` 的区别：
/// 那是**我们自己的**状态目录（`%APPDATA%\claude-hud\sessions`），这是 **Claude Code 的**
/// 数据目录 —— 两个目录名字里都有 "sessions"，别弄混。
///
/// HOME 取不到时回落到 `.claude/projects`（相对路径）：扫描器对"目录不存在"是安静的
/// （账本为 0），所以这条回落只会让数字显不出来，不会让挂件起不来。
pub fn real_projects_dir() -> PathBuf {
    if let Some(over) = std::env::var_os("CLAUDE_HUD_PROJECTS_DIR") {
        return PathBuf::from(over);
    }
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude").join("projects")
}

/// 同一个道理，配置路径也需要可覆盖的真实入口。两个理由：
/// 1. **集成测试不得读写用户真实的 `%APPDATA%\claude-hud\config.json`** ——
///    没有这个覆盖，测试会依赖机器上的真实配置，不 hermetic。
/// 2. pre-flight 裁定要求本任务**改用 `paths::config_path`**，消除此前两处内联
///    `.join("config.json")` 的重复（扫描已把它记为重复项）。
pub fn real_config_path() -> PathBuf {
    if let Some(over) = std::env::var_os("CLAUDE_HUD_CONFIG_PATH") {
        return PathBuf::from(over);
    }
    config_path(&real_base())
}

/// 计划 9b 父进程探针的输出文件。
///
/// 放在 app 目录（`%APPDATA%\claude-hud\`）而不是 `%TEMP%`：它是本应用自己的诊断
/// 产物，目录本来就已经存在（sessions/ 与 config.json 都在里面），不必额外制造
/// 临时文件；探针的 MessageBox 里也会给出完整路径，让人直接打开。
pub fn real_probe_path() -> PathBuf {
    app_dir(&real_base()).join("ppid-probe.txt")
}

/// 探针的**哨兵文件**：存在它 = 安静启用探针（见 `probe::quiet`）。
///
/// 放在 app 目录而不是 `%TEMP%`，理由同上（同一批诊断产物放一起，用户按路径就能打开）。
/// 与 `ppid-probe.txt` 的区别是**读写方向相反**：那个是探针的输出，这个是人的输入。
pub fn real_probe_enable_path() -> PathBuf {
    app_dir(&real_base()).join("ppid-probe.enable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_dir_appends_project_name() {
        let base = Path::new("C:\\base");
        assert_eq!(app_dir(base), PathBuf::from("C:\\base\\claude-hud"));
    }

    #[test]
    fn sessions_dir_nests_under_app_dir() {
        let base = Path::new("C:\\base");
        assert_eq!(
            sessions_dir(base),
            PathBuf::from("C:\\base\\claude-hud\\sessions")
        );
    }

    #[test]
    fn config_path_nests_under_app_dir() {
        let base = Path::new("C:\\base");
        assert_eq!(
            config_path(base),
            PathBuf::from("C:\\base\\claude-hud\\config.json")
        );
    }
}
