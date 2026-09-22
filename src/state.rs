use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub claude_pid: Option<u32>,
    /// 这个会话**是不是被另一个 Claude 会话拉起来的**（`claude -p` 子进程）。
    ///
    /// 三态，因为"没这个字段"和"有、且为假"必须分得开：
    ///
    /// - `Some(true)` —— 宿主之上还有第二个 `claude.exe` ⇒ **子会话**，挂件不列出来。
    /// - `Some(false)` —— 宿主之上没有别的 claude ⇒ 用户自己开的会话。
    /// - `None` —— **改造前写下的状态文件**（2026-09-18 之前没有这个字段）。挂件按
    ///   "用户自己开的"处理（保持改造前的行为）—— **宁可多列一行，不可把真会话藏起来**。
    ///
    /// ⚠️ 老文件之所以读得出 `None`，靠的是 **serde 对 `Option<T>` 的固有行为**（字段缺失
    /// ⇒ `None`），**不是**这一行的 `#[serde(default)]` —— 在 `Option` 上它是冗余的，
    /// 写在这里只为与 `transcript_path` / `claude_pid` 等邻居保持一致。变异验证记过一笔：
    /// 把 `#[serde(default)]` 删掉，全部测试**照样绿**（说明它不承重）；真正承重的是
    /// **三态本身** —— 把缺字段的默认值改成 `Some(false)`（"查过了，不是子会话"），
    /// `a_state_file_written_before_the_nested_field_still_loads` 立刻红。
    ///
    /// 由 hook 在 `SessionStart` 用一次进程表快照定下（那时进程表**本来就要枚举一次**
    /// 抓 `claude_pid`，多问一句零成本）。挂件侧只读，**不在轮询热路径上再枚举进程表**
    /// —— 那是 7.5 ms，见 `procinfo` 顶部那张表。
    #[serde(default)]
    pub nested: Option<bool>,
    pub state: String,
    pub state_since: i64,
    pub last_event: String,
    pub last_event_at: i64,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
    #[serde(default)]
    pub notification_message: Option<String>,
    #[serde(default)]
    pub transcript_offset: u64,
}

fn file_for(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{}.json", session_id))
}

/// 临时文件名对每次调用唯一：把 pid 与线程 id 拼进目标文件名末尾。
/// 固定名（`<id>.json.tmp`）会让同一个 session_id 的两个并发写者共用同一个 tmp ——
/// B 可能在 A 的 `write` 与 `rename` 之间截断该文件，A 随即把截断/空的 tmp rename 到位
/// （挂件就会读到截断的会话状态），而 B 自己的 `rename` 因 tmp 已消失而 `NotFound`。
/// 唯一名同时让"落在旧固定名上的遗留物"不再能阻塞保存。
fn tmp_for(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{:?}.tmp",
        std::process::id(),
        std::thread::current().id()
    ));
    target.with_file_name(name)
}

/// 先写 .tmp 再 rename——rename 在同一卷上是原子的，
/// 保证挂件任何时候读到的都是完整的 JSON，不会读到写了一半的文件。
pub fn save_atomic(dir: &Path, s: &SessionState) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let final_path = file_for(dir, &s.session_id);
    let tmp_path = tmp_for(&final_path);
    let json = serde_json::to_string_pretty(s).unwrap();
    std::fs::write(&tmp_path, json)?;
    // rename 是提交点；失败时必须删掉自己的 tmp —— 唯一名不像固定名那样会被下一次写
    // 覆盖而自愈，留着就会在状态目录里越积越多
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }
    Ok(())
}

pub fn load_one(dir: &Path, session_id: &str) -> Option<SessionState> {
    let s = std::fs::read_to_string(file_for(dir, session_id)).ok()?;
    serde_json::from_str(&s).ok()
}

/// 坏文件跳过而不是让整个挂件崩掉——状态目录里可能有历史遗留或半写文件。
pub fn list_all(dir: &Path) -> Vec<SessionState> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            serde_json::from_str::<SessionState>(&text).ok()
        })
        .collect()
}

pub fn delete(dir: &Path, session_id: &str) -> io::Result<()> {
    match std::fs::remove_file(file_for(dir, session_id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::testtmp::TempDir;

    fn tempdir(tag: &str) -> TempDir {
        TempDir::new("state", tag)
    }

    fn sample(id: &str) -> SessionState {
        SessionState {
            session_id: id.into(),
            cwd: "D:\\projects\\doc-tasks".into(),
            transcript_path: Some("C:\\Users\\user\\.claude\\projects\\x\\y.jsonl".into()),
            display_name: Some("示例项目报告排版修复".into()),
            claude_pid: Some(4242),
            nested: Some(false),
            state: "working".into(),
            state_since: 1000,
            last_event: "UserPromptSubmit".into(),
            last_event_at: 1000,
            last_assistant_message: None,
            notification_message: None,
            transcript_offset: 0,
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let d = tempdir("roundtrip");
        let s = sample("s1");
        save_atomic(&d, &s).unwrap();
        let back = load_one(&d, "s1").unwrap();
        assert_eq!(back.session_id, "s1");
        assert_eq!(back.display_name.as_deref(), Some("示例项目报告排版修复"));
        assert_eq!(back.nested, Some(false), "三态字段必须原样往返");
        // 中文路径必须原样往返（UTF-8 + JSON 转义）
        assert_eq!(back.cwd, "D:\\projects\\doc-tasks");
    }

    #[test]
    fn a_state_file_written_before_the_nested_field_still_loads() {
        // 三态字段的兼容契约（2026-09-18 加 `nested`）：用户盘上此刻就有十几份
        // **没有这个字段**的状态文件，它们必须照旧读得出来，且读出来是 `None`
        // （= "没有这个事实"），不是 `Some(false)`（= "查过了，不是子会话"）。
        //
        // 谁把 `#[serde(default)]` 去掉，这十几份文件就**当场解析失败**；而 `list_all`
        // 对坏文件是**静默跳过**的（见 `corrupt_file_is_skipped_not_fatal`）——
        // 界面上表现成"会话整片消失"，且不报错。
        let d = tempdir("oldfile");
        let old = r#"{"session_id":"old","cwd":"D:\\w","state":"working","state_since":1,
                      "last_event":"Stop","last_event_at":1}"#;
        fs::write(d.join("old.json"), old).unwrap();

        let s = load_one(&d, "old").expect("缺新字段的老文件必须能读");
        assert_eq!(s.session_id, "old");
        assert_eq!(s.nested, None, "没有这个字段 ⇒ None，不是 Some(false)");
        assert_eq!(s.claude_pid, None);
        assert_eq!(list_all(&d).len(), 1, "老文件不能被当成坏文件跳掉");
    }

    #[test]
    fn atomic_write_leaves_no_tmp_file() {
        let d = tempdir("notmp");
        save_atomic(&d, &sample("s2")).unwrap();
        let leftovers: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "原子写不应残留 .tmp");
    }

    #[test]
    fn list_all_returns_every_session() {
        let d = tempdir("listall");
        save_atomic(&d, &sample("a")).unwrap();
        save_atomic(&d, &sample("b")).unwrap();
        let mut ids: Vec<_> = list_all(&d).into_iter().map(|s| s.session_id).collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn list_all_on_missing_dir_is_empty_not_error() {
        // 分两句写：`tempdir("missing").join(..)` 里的临时值会在**语句末尾**析构，
        // 留下的 `PathBuf` 指向一个刚被删掉的目录 —— 虽然本用例故意要"不存在的路径"
        // 因而侥幸仍能通过，但那种写法读起来像 bug。显式绑一个变量才清楚。
        let tmp = tempdir("missing");
        let d = tmp.join("does-not-exist");
        assert!(list_all(&d).is_empty());
    }

    #[test]
    fn corrupt_file_is_skipped_not_fatal() {
        let d = tempdir("corrupt");
        save_atomic(&d, &sample("good")).unwrap();
        fs::write(d.join("bad.json"), b"{ not json").unwrap();
        let ids: Vec<_> = list_all(&d).into_iter().map(|s| s.session_id).collect();
        assert_eq!(ids, vec!["good"]);
    }

    #[test]
    fn delete_removes_file() {
        let d = tempdir("delete");
        save_atomic(&d, &sample("gone")).unwrap();
        // 账本 #31：先断言文件真的落到了盘上，否则本用例**可空过** ——
        // `delete`（NotFound → Ok）与 `load_one`（读不到 → None）都吞掉"文件不存在"，
        // 所以一个"什么都不写"的 `save_atomic` 也能让下面两行全部通过。
        // 变异已验证：把 save_atomic 改成直接 `return Ok(())`，本用例在**这一行**失败
        // （此前它照样通过）。
        assert!(d.join("gone.json").exists(), "前置条件：save_atomic 必须真的写出文件");
        delete(&d, "gone").unwrap();
        assert!(!d.join("gone.json").exists(), "delete 必须真的把文件删掉");
        assert!(load_one(&d, "gone").is_none());
    }

    #[test]
    fn delete_missing_file_is_ok() {
        let d = tempdir("delmissing");
        assert!(delete(&d, "never-existed").is_ok());
    }

    #[test]
    fn overwrite_of_existing_session_file_wins_and_leaves_no_tmp() {
        // 稳态路径：同一个 session 每个 hook 都重写同一个 <id>.json，所以 rename 的目标
        // 通常是"已存在"的文件（Windows 上必须是替换而非失败）。brief 的 7 个测试都写全新目录，
        // 这条路径原先无人钉住。
        let d = tempdir("overwrite");
        let mut first = sample("s1");
        first.display_name = Some("第一版".into());
        first.transcript_offset = 1;
        save_atomic(&d, &first).unwrap();

        let mut second = sample("s1");
        second.display_name = Some("第二版".into());
        second.transcript_offset = 2;
        save_atomic(&d, &second).unwrap();

        let back = load_one(&d, "s1").unwrap();
        assert_eq!(back.display_name.as_deref(), Some("第二版"), "第二次写入必须胜出");
        assert_eq!(back.transcript_offset, 2);
        assert_eq!(back.session_id, "s1");

        let mut names: Vec<String> = fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["s1.json".to_string()], "覆盖写后目录里只应有 s1.json");
    }

    #[test]
    fn stale_tmp_at_the_fixed_name_cannot_break_a_save() {
        // 钉住 progress.md 第 14 条裁定：临时文件名必须每次调用唯一。
        // 若实现退回固定名 `<id>.json.tmp`，落在该路径上的任何遗留物（只读残留、
        // 目录、或并发写者的半成品）都会让保存直接失败 —— 这正是固定名的共享缺口。
        let d = tempdir("tmpcollision");
        let collided = d.join("s1.json.tmp");
        fs::write(&collided, b"stale read-only leftover").unwrap();
        let mut perms = fs::metadata(&collided).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&collided, perms).unwrap();

        // 固定名实现会在此 Err(PermissionDenied)；唯一名实现不碰这个路径，正常保存
        save_atomic(&d, &sample("s1")).unwrap();
        assert_eq!(load_one(&d, "s1").unwrap().session_id, "s1");

        // 收尾：清掉只读位再删除，避免 tempdir 下一轮的 remove_dir_all 被它挡住
        let mut p = fs::metadata(&collided).unwrap().permissions();
        p.set_readonly(false);
        fs::set_permissions(&collided, p).unwrap();
        fs::remove_file(&collided).unwrap();
    }

    #[test]
    fn failed_rename_cleans_up_its_own_tmp() {
        // rename 是提交点；它失败时必须删掉自己的 tmp（裁定第 2 点）。唯一名意味着残留
        // 不再像固定名那样被下一次写覆盖而自愈，不清就会在状态目录里越积越多。
        let d = tempdir("renamefail");
        // 让 rename 必然失败：目标是目录，文件替换不掉目录
        fs::create_dir(d.join("s1.json")).unwrap();

        assert!(save_atomic(&d, &sample("s1")).is_err(), "目标是目录时 rename 必须失败");

        let leftovers: Vec<String> = fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "rename 失败后不应残留 tmp：{leftovers:?}");
    }
}
