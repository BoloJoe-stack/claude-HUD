use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const EVENTS: [&str; 8] = [
    "SessionStart",
    "UserPromptSubmit",
    "Notification",
    "PreCompact",
    "PostCompact",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

pub fn default_settings_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude").join("settings.json")
}

fn exe_str(exe: &Path) -> String {
    exe.to_string_lossy().to_string()
}

/// exec 形式：command 是 exe 绝对路径、args 是参数向量。
/// 这样不经过 shell，既没有引号转义问题，启动也最快。
fn our_group(exe: &Path) -> Value {
    json!([{
        "hooks": [{
            "type": "command",
            "command": exe_str(exe),
            "args": ["--hook"],
            "async": true
        }]
    }])
}

fn is_our_group(grp: &Value, exe: &Path) -> bool {
    grp.pointer("/hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|h| {
                h.get("command").and_then(|c| c.as_str()) == Some(&exe_str(exe))
            })
        })
}

/// 临时文件名对每次调用唯一：把 pid 与线程 id 拼进目标文件名末尾（沿用 state.rs / config.rs
/// 已落地的同一手法）。固定名（`settings.json.tmp`）会让两个并发写者共用同一个 tmp ——
/// B 可能在 A 的 `write` 与 `rename` 之间截断该文件，A 随即把截断的 tmp rename 到位，
/// 而 B 自己的 `rename` 因 tmp 已消失而 `NotFound`。settings.json 是本机所有会话共享的
/// 全局配置，这种失效的爆炸半径比单个会话状态文件更大。
fn tmp_for(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{:?}.tmp",
        std::process::id(),
        std::thread::current().id()
    ));
    target.with_file_name(name)
}

/// 区分三种情况：
/// - 文件不存在 → `Ok({})`：首次安装的正常路径，行为不变
/// - 存在且是合法 JSON object → 原样返回
/// - 存在但读不出来（IO 错 / 非法 JSON / 顶层非 object）→ `Err`
///
/// 第三种必须失败，**绝不能**回落 `{}` 再往下走：settings.json 是用户的全局配置，
/// 「解析不了就当空对象覆盖掉」等于把用户读不懂的内容整份替换成我们的内容——
/// 那正是静默销毁。宁可报错让调用方看见。
fn load(path: &Path) -> std::io::Result<Value> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(e) => return Err(e),
    };
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if !v.is_object() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "settings.json 顶层必须是 JSON object",
        ));
    }
    Ok(v)
}

/// 原子写：先写同目录同卷的唯一名兄弟临时文件，再 `rename` 覆盖目标。
/// 直写在中途被打断时（进程被杀 / 磁盘满）会给用户的全局配置留下半截 JSON，
/// 那会让本机所有会话一起失效；改走 tmp + rename 后，目标要么是旧值要么是新值。
fn store(path: &Path, v: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(v).unwrap();
    let tmp = tmp_for(path);
    std::fs::write(&tmp, json)?;
    // rename 是提交点；失败时必须删掉自己的 tmp —— 唯一名不像固定名那样会被下一次写
    // 覆盖而自愈，留着就会在用户配置目录里越积越多
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

pub fn install(settings_path: &Path, exe: &Path) -> std::io::Result<()> {
    // 读不出来就带着 Err 退出，绝不先回落成 {} 再写回去（见 load 的注释）
    let mut root = load(settings_path)?;

    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();

    for event in EVENTS {
        let slot = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        if !slot.is_array() {
            *slot = json!([]);
        }
        let arr = slot.as_array_mut().unwrap();
        if arr.iter().any(|g| is_our_group(g, exe)) {
            continue; // 幂等：已经装过就不重复加
        }
        for g in our_group(exe).as_array().unwrap() {
            arr.push(g.clone());
        }
    }

    store(settings_path, &root)
}

pub fn uninstall(settings_path: &Path, exe: &Path) -> std::io::Result<()> {
    // 与 install 同理：读不出来必须失败，不得回落 {} 覆盖
    let mut root = load(settings_path)?;
    let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return store(settings_path, &root);
    };

    for event in EVENTS {
        if let Some(arr) = hooks.get_mut(event).and_then(|s| s.as_array_mut()) {
            arr.retain(|g| !is_our_group(g, exe));
            if arr.is_empty() {
                hooks.remove(event);
            }
        }
    }

    store(settings_path, &root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;

    use crate::testtmp::TempDir;

    fn tempdir(tag: &str) -> TempDir {
        TempDir::new("install", tag)
    }

    fn exe() -> PathBuf {
        PathBuf::from("C:\\tools\\claude-hud.exe")
    }

    fn read(p: &PathBuf) -> Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    fn our_entries(v: &Value, exe: &PathBuf) -> usize {
        EVENTS
            .iter()
            .filter_map(|e| v.pointer(&format!("/hooks/{}", e)))
            .filter_map(|g| g.as_array())
            .flatten()
            .filter_map(|grp| grp.pointer("/hooks"))
            .filter_map(|h| h.as_array())
            .flatten()
            .filter(|h| h.get("command").and_then(|c| c.as_str()) == Some(&exe.to_string_lossy()))
            .count()
    }

    #[test]
    fn install_creates_all_eight_events() {
        let d = tempdir("create");
        let p = d.join("settings.json");
        install(&p, &exe()).unwrap();
        let v = read(&p);
        for e in EVENTS {
            assert!(v.pointer(&format!("/hooks/{}", e)).is_some(), "缺 {}", e);
        }
        assert_eq!(our_entries(&v, &exe()), 8);
    }

    #[test]
    fn install_uses_exec_form_without_shell() {
        let d = tempdir("execform");
        let p = d.join("settings.json");
        install(&p, &exe()).unwrap();
        let v = read(&p);
        let h = &v["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(h["type"], "command");
        assert_eq!(h["args"][0], "--hook");
        assert_eq!(h["async"], true);
    }

    #[test]
    fn install_preserves_unrelated_settings() {
        let d = tempdir("preserve");
        let p = d.join("settings.json");
        std::fs::write(&p, r#"{"env":{"FOO":"bar"},"theme":"light"}"#).unwrap();
        install(&p, &exe()).unwrap();
        let v = read(&p);
        assert_eq!(v["env"]["FOO"], "bar", "不能碰用户已有的 env 块");
        assert_eq!(v["theme"], "light");
    }

    #[test]
    fn install_is_idempotent() {
        let d = tempdir("idem");
        let p = d.join("settings.json");
        install(&p, &exe()).unwrap();
        install(&p, &exe()).unwrap();
        install(&p, &exe()).unwrap();
        assert_eq!(our_entries(&read(&p), &exe()), 8, "重复安装不得翻倍");
    }

    #[test]
    fn install_keeps_foreign_hooks() {
        let d = tempdir("foreign");
        let p = d.join("settings.json");
        std::fs::write(
            &p,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"C:\\other.exe"}]}]}}"#,
        )
        .unwrap();
        install(&p, &exe()).unwrap();
        let v = read(&p);
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert!(stop.len() >= 2, "别人的 hook 必须留着");
        assert_eq!(our_entries(&v, &exe()), 8);
    }

    #[test]
    fn uninstall_removes_only_ours() {
        let d = tempdir("uninstall");
        let p = d.join("settings.json");
        std::fs::write(
            &p,
            r#"{"env":{"FOO":"bar"},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"C:\\other.exe"}]}]}}"#,
        )
        .unwrap();
        install(&p, &exe()).unwrap();
        uninstall(&p, &exe()).unwrap();
        let v = read(&p);
        assert_eq!(our_entries(&v, &exe()), 0);
        assert_eq!(v["env"]["FOO"], "bar");
        assert!(v["hooks"]["Stop"].as_array().unwrap().iter().any(|g| {
            g.pointer("/hooks/0/command").and_then(|c| c.as_str())
                == Some("C:\\other.exe")
        }));
    }

    #[test]
    fn uninstall_on_clean_file_is_noop() {
        let d = tempdir("cleanuninstall");
        let p = d.join("settings.json");
        std::fs::write(&p, r#"{"env":{"FOO":"bar"}}"#).unwrap();
        uninstall(&p, &exe()).unwrap();
        assert_eq!(read(&p)["env"]["FOO"], "bar");
    }

    // ── 以下为 Task 7 修复轮新增：钉住「原子写不残留 tmp」与「读不出来必须失败」 ──

    #[test]
    fn install_leaves_no_tmp_of_its_own_beside_a_stale_one() {
        let d = tempdir("notmp");
        let p = d.join("settings.json");
        // 预置一个「旧版本 / 崩溃残留」的固定名陈旧 tmp。唯一名实现不触碰这个路径，
        // 所以它允许留在盘上 —— 但也**仅此一个**：install 自己不得新增任何 tmp。
        // 没有这个预置物，「目录里没有 .tmp」是恒真断言（旧的非原子实现压根不写 tmp，
        // 它直接写目标文件），无法区分实现好坏。
        let stale = d.join("settings.json.tmp");
        std::fs::write(&stale, r#"{"opacity":0.01}"#).unwrap();

        install(&p, &exe()).unwrap();

        let mut leftovers: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        leftovers.sort();
        assert_eq!(
            leftovers,
            vec!["settings.json.tmp".to_string()],
            "install 不应新增 tmp：{leftovers:?}"
        );
        // 预置物既不得被当成提交物，也不得被改写
        assert_eq!(
            std::fs::read_to_string(&stale).unwrap(),
            r#"{"opacity":0.01}"#
        );
        // 目标文件必须是完整可解析的 JSON（原子写的另一半）
        assert!(read(&p).pointer("/hooks/Stop").is_some());
    }

    #[test]
    fn failed_rename_cleans_up_its_own_tmp() {
        let d = tempdir("renamefail");
        let p = d.join("settings.json");
        // 目标存在且是合法 JSON（load 必须成功），但设成只读：Windows 上 rename 覆盖只读
        // 目标会失败，于是 store 走到「rename 是提交点」的失败分支。唯一名 tmp 不会像固定名
        // 那样被下一次写覆盖而自愈，所以该分支必须自己删掉它。
        std::fs::write(&p, r#"{"env":{"FOO":"bar"}}"#).unwrap();
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&p, perms).unwrap();

        let res = install(&p, &exe());

        // 先复位只读位再断言，避免 tempdir 下一轮的 remove_dir_all 被挡住
        let mut open = std::fs::metadata(&p).unwrap().permissions();
        open.set_readonly(false);
        std::fs::set_permissions(&p, open).unwrap();

        assert!(res.is_err(), "目标是只读文件时 rename 必须失败");
        let leftovers: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "rename 失败后不得残留 tmp：{leftovers:?}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            r#"{"env":{"FOO":"bar"}}"#,
            "失败路径不得改动目标文件"
        );
    }

    #[test]
    fn install_on_corrupt_json_fails_and_leaves_bytes_untouched() {
        let d = tempdir("corruptinstall");
        let p = d.join("settings.json");
        // 语法损坏的已存在文件（截断的 JSON）：读不出来
        let original: &[u8] = br#"{"env":{"FOO":"bar"},"theme":"li"#;
        std::fs::write(&p, original).unwrap();

        let res = install(&p, &exe());

        // 字节必须逐字未变 —— 「读不出来就回落 {} 再往下写」等于静默销毁用户的全局配置
        assert_eq!(
            std::fs::read(&p).unwrap(),
            original,
            "install 失败路径不得改动文件一个字节"
        );
        assert!(
            res.is_err(),
            "存在但非法 JSON 时必须 Err，绝不回落 {{}} 去覆盖它"
        );
    }

    #[test]
    fn install_on_non_object_toplevel_fails_and_leaves_bytes_untouched() {
        let d = tempdir("nonobject");
        let p = d.join("settings.json");
        let original: &[u8] = b"[1,2,3]";
        std::fs::write(&p, original).unwrap();

        let res = install(&p, &exe());

        assert_eq!(
            std::fs::read(&p).unwrap(),
            original,
            "顶层非 object 时必须原样留下，不得替换"
        );
        assert!(res.is_err(), "存在但顶层非 object 时必须 Err");
    }

    #[test]
    fn uninstall_on_corrupt_json_fails_and_leaves_bytes_untouched() {
        let d = tempdir("corruptuninstall");
        let p = d.join("settings.json");
        let original: &[u8] = br#"{"hooks":{"Stop":[{"hooks":[{"type":"comm"#;
        std::fs::write(&p, original).unwrap();

        let res = uninstall(&p, &exe());

        assert_eq!(
            std::fs::read(&p).unwrap(),
            original,
            "uninstall 失败路径不得改动文件一个字节"
        );
        assert!(res.is_err(), "uninstall 对读不出来的文件必须 Err");
    }

    #[test]
    fn install_on_missing_file_still_succeeds() {
        // 区分「不存在」与「读不出来」之后，首次安装（文件不存在）必须仍是成功路径
        let d = tempdir("missingok");
        let p = d.join("settings.json");
        assert!(!p.exists());
        install(&p, &exe()).unwrap();
        assert!(read(&p).pointer("/hooks/SessionStart").is_some());
    }
}
