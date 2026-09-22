use crate::config::Config;
use crate::model::{transition, State};
use crate::procinfo;
use crate::state::{self, SessionState};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct HookPayload {
    pub session_id: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    pub cwd: String,
    pub hook_event_name: String,
    #[serde(default)]
    pub permission_mode: Option<String>,

    // Notification
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub notification_type: Option<String>,

    // Stop
    #[serde(default)]
    pub last_assistant_message: Option<String>,

    // SessionStart
    #[serde(default)]
    pub source: Option<String>,
}

impl HookPayload {
    pub fn parse(s: &str) -> serde_json::Result<Self> {
        serde_json::from_str(s)
    }
}

/// 抓一次本进程的 Claude Code 宿主，**顺带**问一句"宿主之上还有没有第二个 claude.exe"
/// （= 本会话是不是另一个会话拉起来的子会话，9b 的落地 + 2026-09-18 的子会话判据）。
/// 返回 `(宿主 pid, 上一层宿主)`。一次快照回答两个问题，第二问**零额外 syscall**。
///
/// ## 抓的时机
///
/// 主要靠 `SessionStart`（每会话一次），两个理由：
///
/// 1. 宿主 pid 对同一会话**恒定** —— 2026-09-15 取证：一个会话 3/3、另一个 2/2
///    （`docs/工作日志.md` 有原始样本）。抓一次就够。
/// 2. `procinfo::snapshot()` 实测 **7.5 ms**（release 与 debug 一样，是内核枚举进程表的
///    代价）。放进**每个** hook 事件会让 hook 慢 4~5 倍 —— 它本来约 2 ms。
///
/// `SessionStart` 在**新开与 resume** 时都会触发（payload 的 `source` 区分），所以
/// "同一个 session_id 换了宿主进程"这一情形也由它覆盖。
/// 另有**没抓到 pid 时的补抓**，见 [`should_capture`]。
///
/// 拿不到就返回 `None`（**不猜**），调用方保留原值。挂件侧对"没有 pid"的处理是
/// "不做僵尸判定" —— 退回改造前的行为。**宁可漏报僵尸，不可误杀活人。**
///
/// 子会话那半的判据与真机形态见 `procinfo::host_and_parent`。
fn capture_hosts() -> Option<(u32, Option<u32>)> {
    let me = unsafe { procinfo::GetCurrentProcessId() };
    let table = procinfo::snapshot().ok()?;
    procinfo::host_and_parent(&table, me, procinfo::HOST_EXE)
}

/// 这一轮该不该去抓宿主（纯函数，理由都写在下面，好单独钉住）。
///
/// - **`SessionStart` 必抓**：新开与 resume 都走它，宿主这时才知道。
/// - **其余事件只在还没抓到时抓**（`prev_pid.is_none()`）。此前不补抓，于是
///   "`SessionStart` 那次快照失败 / 状态文件是改造前留下的"会话**永远拿不到 pid**，
///   而挂件对没有 pid 的会话**不做存活判定**（`host_is_gone` 的安全阀）——
///   它退出后会**永久留在列表里**。实测 2026-09-18：用户 4 个会话里就有一个是这种
///   （`样例系统`）。
/// - 抓到一次就停 ⇒ 这不是热路径上的常驻开销；只有真正缺 pid 的会话才多付那 7.5 ms，
///   而 hook 全是 `async: true`，本来也不阻塞 Claude Code。
///
/// **已经有 pid 就不再抓**：改造前留下的状态文件 `nested` 会因此一直是 `None`，挂件对它
/// 按"用户自己开的"处理。这是有意的 —— **子会话按定义都是新开出来的**（一个老会话不可能
/// 凭空变成子会话），所以新会话必定在 `SessionStart` 拿到这个事实；为几份老文件反复枚举
/// 进程表不值得，何况"多列一行"比"藏掉一个真会话"轻。
fn should_capture(event: &str, prev_pid: Option<u32>) -> bool {
    event == "SessionStart" || prev_pid.is_none()
}

pub fn handle(
    p: &HookPayload,
    sessions_dir: &Path,
    cfg: &Config,
    now: i64,
) -> std::io::Result<()> {
    let existing = state::load_one(sessions_dir, &p.session_id);

    let current = existing
        .as_ref()
        .map(|s| State::from_str(&s.state))
        .unwrap_or(State::Idle);

    let t = transition(current, p, cfg, now);

    if t.delete {
        return state::delete(sessions_dir, &p.session_id);
    }

    let state_changed = t.state != current;
    let prev = existing.unwrap_or(SessionState {
        session_id: p.session_id.clone(),
        cwd: p.cwd.clone(),
        transcript_path: None,
        display_name: None,
        claude_pid: None,
        nested: None,
        state: State::Idle.as_str().to_string(),
        state_since: now,
        last_event: String::new(),
        last_event_at: now,
        last_assistant_message: None,
        notification_message: None,
        transcript_offset: 0,
    });

    // 9b：宿主 pid 与"是不是子会话"在 `SessionStart` 抓一次，其余事件沿用（缺 pid 时补抓）。
    // 见 `capture_hosts` / `should_capture`。
    let (claude_pid, nested) = if should_capture(&p.hook_event_name, prev.claude_pid) {
        match capture_hosts() {
            Some((host, above)) => (Some(host), Some(above.is_some())),
            // 抓不到就**原样保留**两条旧值 —— 宁可漏报，不可误判（见 `capture_hosts` 的注释）
            None => (prev.claude_pid, prev.nested),
        }
    } else {
        (prev.claude_pid, prev.nested)
    };

    let next = SessionState {
        session_id: p.session_id.clone(),
        cwd: p.cwd.clone(),
        // payload 给了就更新，没给就保留——SessionStart 之后的事件不一定带 transcript_path
        transcript_path: p
            .transcript_path
            .clone()
            .or(prev.transcript_path.clone()),
        display_name: prev.display_name.clone(),
        claude_pid,
        nested,
        state: t.state.as_str().to_string(),
        // 只在状态真的变化时重置计时，否则计时器会被无关事件反复清零
        state_since: if state_changed { now } else { prev.state_since },
        last_event: p.hook_event_name.clone(),
        last_event_at: now,
        last_assistant_message: p
            .last_assistant_message
            .clone()
            .or(prev.last_assistant_message.clone()),
        notification_message: p
            .message
            .clone()
            .or(prev.notification_message.clone()),
        transcript_offset: prev.transcript_offset,
    };

    state::save_atomic(sessions_dir, &next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_notification_payload() {
        let raw = r#"{
            "session_id": "abc123",
            "transcript_path": "/Users/me/.claude/projects/x/y.jsonl",
            "cwd": "/Users/me",
            "hook_event_name": "Notification",
            "message": "Claude needs your permission",
            "title": "Permission needed",
            "notification_type": "permission_prompt"
        }"#;
        let p = HookPayload::parse(raw).unwrap();
        assert_eq!(p.session_id, "abc123");
        assert_eq!(p.hook_event_name, "Notification");
        assert_eq!(p.notification_type.as_deref(), Some("permission_prompt"));
        assert_eq!(p.title.as_deref(), Some("Permission needed"));
    }

    #[test]
    fn parses_session_start_with_source() {
        let raw = r#"{
            "session_id": "s1",
            "cwd": "D:\\work",
            "hook_event_name": "SessionStart",
            "source": "startup"
        }"#;
        let p = HookPayload::parse(raw).unwrap();
        assert_eq!(p.source.as_deref(), Some("startup"));
        assert!(p.transcript_path.is_none());
    }

    #[test]
    fn parses_stop_with_last_assistant_message() {
        let raw = r#"{
            "session_id": "s2",
            "cwd": "D:\\work",
            "hook_event_name": "Stop",
            "last_assistant_message": "已修复排版，共 12 处"
        }"#;
        let p = HookPayload::parse(raw).unwrap();
        assert_eq!(p.last_assistant_message.as_deref(), Some("已修复排版，共 12 处"));
    }

    #[test]
    fn parses_minimal_pretooluse_shaped_payload() {
        // 未知事件名也不能解析失败——多出来的字段要被忽略
        let raw = r#"{"session_id":"s3","cwd":"D:\\work","hook_event_name":"UserPromptSubmit","extra":123}"#;
        let p = HookPayload::parse(raw).unwrap();
        assert_eq!(p.hook_event_name, "UserPromptSubmit");
    }

    #[test]
    fn missing_required_fields_are_an_error_not_a_default() {
        // 账本 #19：本结构区别于"静默有损解析器"的**唯一**行为，就是缺 session_id /
        // cwd / hook_event_name 时返回 Err。而它此前只由"没写某个 serde 属性"保证
        // —— 谁"宽容一点"补上一个 `#[serde(default)]`，缺 session_id 的事件就会全部
        // 落到同一个（空）文件名上（`state::file_for` 拼出 `.json`），把多个会话串成
        // 一个，而且**不会报错**（正是 spec §4.3 点名的那类失效）。
        // 三字段齐全必须能解析，缺任意一个必须 Err：两个方向都钉住，防止"干脆全
        // 判 Err"或"干脆全给默认值"这两种相反的走样。
        let full = r#"{"session_id":"s","cwd":"D:\\work","hook_event_name":"Stop"}"#;
        assert!(HookPayload::parse(full).is_ok(), "三字段齐全时必须能解析");
        assert_eq!(HookPayload::parse(full).unwrap().session_id, "s");

        for (missing, raw) in [
            ("session_id", r#"{"cwd":"D:\\work","hook_event_name":"Stop"}"#),
            ("cwd", r#"{"session_id":"s","hook_event_name":"Stop"}"#),
            ("hook_event_name", r#"{"session_id":"s","cwd":"D:\\work"}"#),
        ] {
            assert!(
                HookPayload::parse(raw).is_err(),
                "缺 {missing} 必须返回 Err，不得填默认值"
            );
        }
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    #[test]
    fn the_host_is_recaptured_while_a_session_still_has_no_pid() {
        // `SessionStart` 必抓（新开与 resume 都走它）。
        assert!(should_capture("SessionStart", None));
        assert!(should_capture("SessionStart", Some(1234)), "resume 可能换了宿主");

        // 已经有 pid 的普通事件：不再枚举进程表 —— 快照 7.5 ms，hook 本来只花约 2 ms。
        assert!(!should_capture("UserPromptSubmit", Some(1234)));
        assert!(!should_capture("Stop", Some(1234)));
        assert!(!should_capture("SessionEnd", Some(1234)));

        // 还没 pid 的：**一直有机会补上**。这条是 2026-09-18 修的那个"永久残留"——
        // 不补抓，这类会话就永远没有宿主，也就永不做存活判定，退出后一直留在列表里
        // （真机上 `样例系统` 那一行就是这种）。
        for ev in ["UserPromptSubmit", "Stop", "Notification", "PreCompact"] {
            assert!(should_capture(ev, None), "{ev}：缺 pid 时必须补抓");
        }
    }
}

#[cfg(test)]
mod handle_tests {
    use super::*;
    use crate::config::Config;
    use crate::state;

    use crate::testtmp::TempDir;

    fn tempdir(tag: &str) -> TempDir {
        TempDir::new("handle", tag)
    }

    fn payload(event: &str, notif: Option<&str>, msg: Option<&str>) -> HookPayload {
        let mut p: HookPayload = HookPayload::parse(&format!(
            r#"{{"session_id":"s1","cwd":"D:\\w","hook_event_name":"{}"}}"#,
            event
        ))
        .unwrap();
        p.notification_type = notif.map(str::to_string);
        p.last_assistant_message = msg.map(str::to_string);
        p.transcript_path = Some("C:\\t.jsonl".into());
        p
    }

    #[test]
    fn session_start_creates_file() {
        let d = tempdir("start");
        handle(&payload("SessionStart", None, None), &d, &Config::default(), 1000).unwrap();
        let s = state::load_one(&d, "s1").unwrap();
        assert_eq!(s.state, "idle");
        assert_eq!(s.cwd, "D:\\w");
        assert_eq!(s.transcript_path.as_deref(), Some("C:\\t.jsonl"));
    }

    #[test]
    fn user_prompt_submit_moves_to_working() {
        let d = tempdir("working");
        handle(&payload("SessionStart", None, None), &d, &Config::default(), 1000).unwrap();
        handle(&payload("UserPromptSubmit", None, None), &d, &Config::default(), 1200).unwrap();
        let s = state::load_one(&d, "s1").unwrap();
        assert_eq!(s.state, "working");
        assert_eq!(s.last_event, "UserPromptSubmit");
        assert_eq!(s.last_event_at, 1200);
    }

    #[test]
    fn state_since_only_moves_when_state_actually_changes() {
        // 计时必须反映"当前状态持续了多久"。连续两条都处于 working 的事件
        // 不能把计时器重置掉，否则进度看起来永远是 00:00。
        let d = tempdir("since");
        handle(&payload("UserPromptSubmit", None, None), &d, &Config::default(), 1000).unwrap();
        handle(&payload("Notification", Some("other"), None), &d, &Config::default(), 1500).unwrap();
        let s = state::load_one(&d, "s1").unwrap();
        assert_eq!(s.state, "working");
        assert_eq!(s.state_since, 1000, "状态没变，state_since 不该动");
        assert_eq!(s.last_event_at, 1500, "但 last_event_at 要更新");
    }

    #[test]
    fn permission_notification_records_message() {
        let d = tempdir("notif");
        let p = payload("Notification", Some("permission_prompt"), None);
        handle(&p, &d, &Config::default(), 2000).unwrap();
        let s = state::load_one(&d, "s1").unwrap();
        assert_eq!(s.state, "waiting");
    }

    #[test]
    fn stop_records_last_assistant_message() {
        let d = tempdir("stop");
        handle(&payload("SessionStart", None, None), &d, &Config::default(), 1000).unwrap();
        handle(&payload("Stop", None, Some("已修复排版，共 12 处")), &d, &Config::default(), 1100).unwrap();
        let s = state::load_one(&d, "s1").unwrap();
        assert_eq!(s.state, "done");
        assert_eq!(s.last_assistant_message.as_deref(), Some("已修复排版，共 12 处"));
    }

    #[test]
    fn session_end_deletes_file() {
        let d = tempdir("end");
        handle(&payload("SessionStart", None, None), &d, &Config::default(), 1000).unwrap();
        handle(&payload("SessionEnd", None, None), &d, &Config::default(), 1100).unwrap();
        assert!(state::load_one(&d, "s1").is_none(), "SessionEnd 必须删掉状态文件");
    }

    #[test]
    fn transcript_offset_survives_across_events() {
        // ⚠️ **这条测试的理由已经被推翻了，但断言要留。**
        //
        // 原先的理由（"hook 清零 ⇒ 每来一个事件挂件都要重扫整个 transcript"）**不再
        // 成立**：Ruling #6 落地后，挂件每轮**无条件**用内存 map 覆盖这个字段
        // （`ui.rs` 的 `poll_once`：`s.transcript_offset = offsets.get(..).unwrap_or(0)`），
        // **盘上的值永远进不了 `read_new`** —— 即盘上的 offset 目前**没有任何读者**。
        //
        // 那为什么还留这条断言？因为它是**状态文件 schema 的稳定性契约**：hook 是这些
        // 文件唯一的写者，它不该单方面抹掉一个自己不理解其用途的字段。谁将来要基于
        // 盘上的 offset 做判断（9b 僵尸回收、多实例挂件），先读这段：**它现在不是真值。**
        let d = tempdir("offset");
        handle(&payload("SessionStart", None, None), &d, &Config::default(), 1000).unwrap();
        let mut s = state::load_one(&d, "s1").unwrap();
        s.transcript_offset = 524_288;
        state::save_atomic(&d, &s).unwrap();
        handle(&payload("UserPromptSubmit", None, None), &d, &Config::default(), 1100).unwrap();
        assert_eq!(state::load_one(&d, "s1").unwrap().transcript_offset, 524_288);
    }
}
