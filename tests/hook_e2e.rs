use std::io::Write;
use std::process::{Command, Stdio};

/// 测试临时目录，`Drop` 时递归删除。
///
/// ⚠️ 这是 `src/testtmp.rs` 的**必要重复**，不是忘记复用：本 crate 只有 bin target、
/// 没有 lib target，集成测试因此**无法** `use` `src/` 里的任何模块。
/// **改一处必须同时改两处** —— 两边都是"创建后不回收"的老毛病留下的 3090 个目录之一。
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("claude-hud-e2e-{}-{seq}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建测试临时目录失败");
        Self(dir)
    }
}

impl std::ops::Deref for TempDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::path::Path> for TempDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // 删不掉就算了，绝不能在栈展开里 panic。
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempdir(tag: &str) -> TempDir {
    TempDir::new(tag)
}

fn run_hook(sessions_dir: &std::path::Path, payload: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_claude-hud"))
        .arg("--hook")
        .env("CLAUDE_HUD_SESSIONS_DIR", sessions_dir)
        // 只设 SESSIONS_DIR 时 `real_config_path()` 仍会指到用户真实的
        // `%APPDATA%\claude-hud\config.json` —— 测试就依赖了机器上的真实配置，
        // 不 hermetic（本任务裁定：集成测试不得读写用户真实的配置与状态目录）。
        // 配置一并指到临时目录后，两条路径都只落在 tempdir 里。
        .env("CLAUDE_HUD_CONFIG_PATH", sessions_dir.join("e2e-config.json"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn claude-hud --hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "hook 必须以 0 退出，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn full_lifecycle_writes_then_removes_state_file() {
    let d = tempdir("lifecycle");

    run_hook(
        &d,
        r#"{"session_id":"e2e-1","cwd":"D:\\w","hook_event_name":"SessionStart","source":"startup"}"#,
    );
    let f = d.join("e2e-1.json");
    assert!(f.exists(), "SessionStart 后应出现状态文件");

    run_hook(
        &d,
        r#"{"session_id":"e2e-1","cwd":"D:\\w","hook_event_name":"UserPromptSubmit"}"#,
    );
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
    assert_eq!(v["state"], "working");

    // 中文必须原样往返（真实环境里 cwd 全是中文路径）
    run_hook(
        &d,
        r#"{"session_id":"e2e-1","cwd":"D:\\projects\\doc-tasks","hook_event_name":"Stop","last_assistant_message":"已修复排版，共 12 处"}"#,
    );
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
    assert_eq!(v["state"], "done");
    assert_eq!(v["cwd"], "D:\\projects\\doc-tasks");
    assert_eq!(v["last_assistant_message"], "已修复排版，共 12 处");

    run_hook(
        &d,
        r#"{"session_id":"e2e-1","cwd":"D:\\w","hook_event_name":"SessionEnd"}"#,
    );
    assert!(!f.exists(), "SessionEnd 后状态文件必须消失");
}

#[test]
fn two_sessions_do_not_collide() {
    let d = tempdir("two");
    run_hook(&d, r#"{"session_id":"a","cwd":"D:\\wa","hook_event_name":"UserPromptSubmit"}"#);
    run_hook(&d, r#"{"session_id":"b","cwd":"D:\\wb","hook_event_name":"UserPromptSubmit"}"#);
    assert!(d.join("a.json").exists());
    assert!(d.join("b.json").exists());
    let a: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(d.join("a.json")).unwrap()).unwrap();
    assert_eq!(a["cwd"], "D:\\wa", "两个会话的 cwd 不能串");
}
