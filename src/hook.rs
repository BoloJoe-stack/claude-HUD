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

/// 一次抓到的宿主事实：**是谁**（pid）、**它上面还有没有另一个宿主**（子会话判据）、
/// **它的创建时间**（身份，防 pid 回收）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HostCapture {
    pid: u32,
    above: Option<u32>,
    start: Option<u64>,
}

/// 抓一次本进程的 Claude Code 宿主，**顺带**问两件事："宿主之上还有没有第二个 claude.exe"
/// （= 本会话是不是另一个会话拉起来的子会话，9b 的落地 + 2026-09-18 的子会话判据）、
/// 以及"宿主的创建时间"（2026-09-23 的身份判据）。一次快照回答前两问，第三问**同一个
/// 句柄再加一次廉价调用**（微秒级）。
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
/// 子会话那半的判据与真机形态见 `procinfo::host_and_parent`；
/// 创建时间那半见 [`crate::state::SessionState::claude_start`]。
fn capture_hosts() -> Option<HostCapture> {
    let me = unsafe { procinfo::GetCurrentProcessId() };
    let table = procinfo::snapshot().ok()?;
    let (pid, above) = procinfo::host_and_parent(&table, me, procinfo::HOST_EXE)?;
    // 创建时间取不到就记 `None`（**不猜**，也不拦着这次抓捕）：状态文件退回"只看 pid +
    // 映像名"的老判据 —— 宁可漏报僵尸，不可误杀活人。
    let start = procinfo::start_time(pid);
    Some(HostCapture { pid, above, start })
}

/// 把一次抓捕的结果并进旧值：**三个字段整组换，或者整组不动**。
///
/// 抓不到（`captured == None`）时三条**原样保留**——与 [`capture_hosts`] 的"不猜"一致。
///
/// ## 为什么必须整组（这条比它看起来重要）
///
/// `pid` 与 `start` 是**同一个事实的两半**：一个 pid 配**另一个进程**的创建时间，会让
/// [`crate::ui`] 的存活判定把**活着的**会话判成已退出（pid 对得上、时间对不上 ⇒ 判死）。
/// 所以"新 pid + 旧时间"这种半新半旧的写法是**唯一会伤到真会话**的写法，此处用返回
/// 三元组一次写清、并由 `merge_host_is_all_or_nothing` 钉住。
fn merge_host(
    captured: Option<HostCapture>,
    prev: (Option<u32>, Option<bool>, Option<u64>),
) -> (Option<u32>, Option<bool>, Option<u64>) {
    match captured {
        Some(h) => (Some(h.pid), Some(h.above.is_some()), h.start),
        None => prev,
    }
}

// ---- 僵尸状态文件的清扫（2026-09-23）-----------------------------------------

/// 清扫前先等这么久：宿主进程已经消失、且**最后一条事件也早于**这个时长的状态文件才算僵尸。
///
/// 为什么要等：hook 全是 `async: true`，一个刚结束的会话可能还有事件在写途中；而且挂件
/// 自己就会在连续两轮（约 2 s）内把"已退出"的行撤掉 —— 文件在那一小会儿里没有任何用处。
/// 一小时是个**保守**值：清扫只负责"别让僵尸无界堆积"，不负责抢那几秒钟。
const SWEEP_GRACE_SECS: i64 = 3600;

/// 宿主**确已不在**吗？（与挂件侧 `ui::host_is_gone` 同一条判据、同一个取向。）
///
/// 只有拿到**正面证据**才回答 `true`：pid 不存在、或那个号上跑的是别的程序、或创建时间
/// 对不上。`claude_pid` 为 `None`（老文件 / 没抓到）时一律 `false` —— **不判定**。
/// 判不准（权限不足等）时 [`procinfo::alive`] 已经判活，所以这里也不会误删。
///
/// `exe_name` 是参数而不是写死 `procinfo::HOST_EXE`，理由与 `ui::host_is_gone` 那条一样：
/// **让这条判据在单测里走得到身份那一级**（测试进程不叫 `claude.exe`）。
fn confirmed_dead(s: &SessionState, exe_name: &str) -> bool {
    match s.claude_pid {
        None => false,
        Some(pid) => !procinfo::alive(pid, exe_name, s.claude_start),
    }
}

/// 删掉"确认已死且已经静了很久"的状态文件，返回删掉的份数。
///
/// ## 为什么由 hook 干这件事
///
/// **挂件绝不写状态文件**（Ruling #6：一个状态文件只能有一个写者）—— 删也是写。hook 本来
/// 就是这些文件唯一的写者（`state::save_atomic` / `state::delete`），清扫放在它这里，
/// 那条边界一条都不用破。
///
/// ## 为什么必须有这件事
///
/// 没有清扫，状态文件只增不减：用户盘上此刻就有 15 份宿主早已退出的僵尸文件（其中 4 份是
/// 09-18 那次实验的子会话，标题 `estimate.rs 成本模块…`）。僵尸本身只是脏，**但它配上
/// pid 回收就会冒到桌面上** —— 2026-09-23 用户报的"凭空冒出一个会话"就是这么来的
/// （现场见 `docs/工作日志.md` 2026-09-23 那节）。`claude_start` 让复活**判得出来**，
/// 清扫让这些文件**根本不再留在盘上**：两道一起上，才算把这条账收了。
///
/// 只在 `SessionStart` 调（每会话一次）：它要读一遍状态目录、对每份文件开一次进程句柄，
/// 都是微秒级，但没理由放进每个事件。
fn sweep_dead(dir: &Path, now: i64, exe_name: &str) -> usize {
    let mut removed = 0;
    for s in state::list_all(dir) {
        if now.saturating_sub(s.last_event_at) < SWEEP_GRACE_SECS {
            continue;
        }
        if !confirmed_dead(&s, exe_name) {
            continue;
        }
        if state::delete(dir, &s.session_id).is_ok() {
            removed += 1;
        }
    }
    removed
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

/// 抓宿主的那个动作。**注入**是为了让"写盘"这一段在单测里也走得到 ——
/// 见 `handle_with` 的注释（与 `cursor::screen_pos` 同一个手法）。
type HostProbe = dyn Fn() -> Option<HostCapture>;

pub fn handle(
    p: &HookPayload,
    sessions_dir: &Path,
    cfg: &Config,
    now: i64,
) -> std::io::Result<()> {
    handle_with(p, sessions_dir, cfg, now, &capture_hosts)
}

/// `handle` 的本体，宿主怎么抓由调用方给。
///
/// 生产走 [`capture_hosts`]（真去枚举进程表）。测试注入一个**假的** `HostCapture` ——
/// 否则单测里永远抓不到宿主（`cargo` 起的测试进程上面没有 `claude.exe`），于是**写盘那几行
/// 一行都测不到**：`claude_start` 忘了写进 `next`、或者写成"新 pid 配旧时间"，测试
/// 照样全绿，而线上挂件会把**活着的**会话判死。这类"接线错了没人发现"的洞，
/// 本项目已经栽过（`a_long_name_never_runs_into_the_right_group` 那轮的账）。
fn handle_with(
    p: &HookPayload,
    sessions_dir: &Path,
    cfg: &Config,
    now: i64,
    capture: &HostProbe,
) -> std::io::Result<()> {
    let existing = state::load_one(sessions_dir, &p.session_id);

    // 清扫僵尸状态文件，只在 `SessionStart` 做一次（见 `sweep_dead`）。
    // 放在读 `existing` 之后：本会话自己的文件此刻若还是**上一轮**的残留（resume），
    // 它同样该被扫掉 —— 反正下面会把它整份重写。
    if p.hook_event_name == "SessionStart" {
        sweep_dead(sessions_dir, now, procinfo::HOST_EXE);
    }

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
        claude_start: None,
        nested: None,
        state: State::Idle.as_str().to_string(),
        state_since: now,
        last_event: String::new(),
        last_event_at: now,
        last_assistant_message: None,
        notification_message: None,
        transcript_offset: 0,
    });

    // 9b：宿主 pid、宿主之上有没有第二个 claude、以及宿主的创建时间，都在 `SessionStart`
    // 抓一次，其余事件沿用（缺 pid 时补抓）。见 `capture_hosts` / `merge_host` /
    // `should_capture`。
    let captured = if should_capture(&p.hook_event_name, prev.claude_pid) {
        capture()
    } else {
        None
    };
    // 抓不到就**原样保留**（含创建时间）—— 宁可漏报，不可误判（见 `capture_hosts` 的注释）
    let (claude_pid, nested, claude_start) = merge_host(
        captured,
        (prev.claude_pid, prev.nested, prev.claude_start),
    );

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
        claude_start,
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

    #[test]
    fn merge_host_is_all_or_nothing() {
        // 抓到 ⇒ **三个字段整组换新**；抓不到 ⇒ **整组原样保留**。
        //
        // 半新半旧（新 pid 配旧创建时间）是这套判据唯一能伤到**真会话**的写法：挂件那边
        // 会判"pid 在、但不是我记的那个进程"，于是一个活着的会话被标成已退出、两轮后从
        // 界面上消失。所以这条用例盯的是"有没有人把某一条路径写成了各更各的"。
        let prev = (Some(111), Some(false), Some(999));
        assert_eq!(merge_host(None, prev), prev, "抓不到 ⇒ 一个字段都不许动");

        let got = merge_host(
            Some(HostCapture { pid: 222, above: Some(333), start: Some(888) }),
            prev,
        );
        assert_eq!(got, (Some(222), Some(true), Some(888)), "抓到 ⇒ 整组换成新的");

        // 同上，但 `above` 为空（用户自己开的会话）且创建时间没取到：三个字段仍然一起动。
        let got = merge_host(
            Some(HostCapture { pid: 444, above: None, start: None }),
            prev,
        );
        assert_eq!(
            got,
            (Some(444), Some(false), None),
            "取不到创建时间也要整组换 —— 留着旧时间就变成'新 pid 配旧时间'"
        );
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

    /// 一个**不可能存在**的 pid：Windows 的 pid 是 4 的倍数，且远小于 `u32::MAX`。
    /// （`ui.rs` 里那条僵尸用例用的是同一个手法。）
    const NO_SUCH_PID: u32 = u32::MAX - 3;

    /// 写一份状态文件，只指定清扫关心的那几个字段。
    fn write(dir: &Path, id: &str, pid: Option<u32>, start: Option<u64>, last_event_at: i64) {
        let s = SessionState {
            session_id: id.into(),
            cwd: "D:\\w".into(),
            transcript_path: None,
            display_name: None,
            claude_pid: pid,
            claude_start: start,
            nested: Some(false),
            state: "done".into(),
            state_since: last_event_at,
            last_event: "Stop".into(),
            last_event_at,
            last_assistant_message: None,
            notification_message: None,
            transcript_offset: 0,
        };
        state::save_atomic(dir, &s).unwrap();
    }

    /// 本测试二进制的映像名（清扫的身份判据要拿真名去比，见 `confirmed_dead`）。
    fn my_exe_name() -> String {
        std::env::current_exe()
            .expect("测试二进制必然有路径")
            .file_name()
            .expect("必然有文件名")
            .to_string_lossy()
            .to_string()
    }

    fn remaining(dir: &Path) -> Vec<String> {
        let mut ids: Vec<String> = state::list_all(dir).into_iter().map(|s| s.session_id).collect();
        ids.sort();
        ids
    }

    #[test]
    fn sweep_removes_only_files_whose_host_is_confirmed_gone() {
        // 2026-09-23 的账：状态文件只增不减，而僵尸配 pid 回收就会冒到桌面上。
        // 这条用例把"什么该删、什么绝不能删"一次全钉住。
        //
        // 拿**本测试进程**当"活着的宿主"：真 pid、真创建时间，所以创建时间那一级在单测里
        // 也走得到（测试进程的映像名不是 claude.exe，故 exe 名由 `my_exe_name` 给）。
        const NOW: i64 = 1_800_000_000;
        const LONG_AGO: i64 = NOW - 86_400;
        let d = tempdir("sweep");
        let me = std::process::id();
        let exe = my_exe_name();
        let real = procinfo::start_time(me).expect("本进程必须有创建时间");

        write(&d, "alive", Some(me), Some(real), LONG_AGO); // 宿主还在（身份也对） → 留
        write(&d, "alive-oldfile", Some(me), None, LONG_AGO); // 老文件没记创建时间 → 留
        write(&d, "no-pid", None, None, LONG_AGO); // 不判定 → 留
        write(&d, "dead", Some(NO_SUCH_PID), None, LONG_AGO); // 宿主进程没了 → 删
        write(&d, "recycled", Some(me), Some(real + 1), LONG_AGO); // **僵尸复活那条路** → 删
        write(&d, "fresh-dead", Some(NO_SUCH_PID), None, NOW - 60); // 刚结束，还在宽限期 → 留

        let removed = sweep_dead(&d, NOW, &exe);
        assert_eq!(removed, 2, "只该删两份（dead 与 recycled）");
        assert_eq!(
            remaining(&d),
            vec!["alive", "alive-oldfile", "fresh-dead", "no-pid"],
            "活着的、老格式的、没 pid 的、刚结束的 —— 一份都不许动"
        );
    }

    #[test]
    fn the_sweep_reads_a_wrong_image_name_as_evidence_not_as_ignorance() {
        // 两种"看起来都没结论"的形态，处理**正好相反**（口径来自 `procinfo::alive`）：
        //
        // - 那个 pid 上跑着**别的程序** → **正面证据**：宿主确实没了 ⇒ 清扫**必须**删；
        // - 根本**问不出来**（权限不足、调用失败）→ 没有证据 ⇒ 判活 ⇒ 不许删。
        //
        // 谁把前者也归进"判不准"里跳过，僵尸就永远清不掉；谁把后者当成"确实死了"，
        // 就会把活着的会话连文件一起删掉。同一个 pid、同一个文件，只用**不同的 exe 名**
        // 去判，结论必须相反 —— 这条区别就在这一个断言里。
        //
        // （"问不出来"那一支没法在这里稳定构造：它要求拿到一个**存在但打不开**的 pid。
        // 真机上就是系统进程那种情形，见 `procinfo::alive` 里 ERROR_ACCESS_DENIED 那段。）
        const NOW: i64 = 1_800_000_000;
        let d = tempdir("sweep-exe-name");
        let me = std::process::id();
        let exe = my_exe_name();
        write(&d, "live-host", Some(me), None, NOW - 86_400);

        assert_eq!(sweep_dead(&d, NOW, &exe), 0, "名字对得上、进程还在 ⇒ 不许删");
        assert_eq!(remaining(&d), vec!["live-host"]);

        assert_eq!(
            sweep_dead(&d, NOW, "definitely-not-this.exe"),
            1,
            "映像名不符 = 那个号上是别的程序 = 确认已死 ⇒ 必须删"
        );
        assert!(remaining(&d).is_empty());
    }

    #[test]
    fn the_sweep_runs_only_on_session_start() {
        // 清扫要读一遍状态目录、对每份文件开一次进程句柄 —— 都是微秒级，但没理由放进
        // 每个事件（hook 每轮还要跑上百次）。这条用例钉住"只有 SessionStart 会清"。
        let d = tempdir("sweep-event");
        let me = std::process::id();
        let real = procinfo::start_time(me).expect("本进程必须有创建时间");

        // 一份确认已死的僵尸：pid 真实存在，但**创建时间对不上** ⇒ 就是被回收的号。
        write(&d, "zombie", Some(me), Some(real + 1), 1000);

        // 非 SessionStart 的事件：不许清（这里连状态文件都还在）
        for ev in ["UserPromptSubmit", "Stop", "Notification"] {
            handle(&payload(ev, None, None), &d, &Config::default(), 1_800_000_000).unwrap();
            assert_eq!(
                remaining(&d),
                vec!["s1", "zombie"],
                "{ev}：只有 SessionStart 才清扫（这份僵尸必须还在）"
            );
        }

        handle(&payload("SessionStart", None, None), &d, &Config::default(), 1_800_000_000)
            .unwrap();
        assert_eq!(remaining(&d), vec!["s1"], "SessionStart 必须把它清掉");
    }

    #[test]
    fn session_start_writes_the_host_identity_and_later_events_keep_it() {
        // **写盘那一段**（单测里抓不到真宿主，所以注入一个假的）：`SessionStart` 抓到的
        // (pid, 创建时间, 是不是子会话) 必须**三个一起**落进状态文件；之后的事件不再抓
        // （省那 7.5 ms），但也**一个都不许丢**。
        let d = tempdir("capture-write");
        let cfg = Config::default();
        let first = || Some(HostCapture { pid: 4242, above: None, start: Some(777) });
        handle_with(&payload("SessionStart", None, None), &d, &cfg, 1000, &first).unwrap();

        let s = state::load_one(&d, "s1").unwrap();
        assert_eq!(s.claude_pid, Some(4242), "宿主的 pid 必须落盘");
        assert_eq!(s.claude_start, Some(777), "创建时间必须与 pid 一起落盘");
        assert_eq!(s.nested, Some(false), "`above` 为空 ⇒ 用户自己开的");

        // 第二个会话是**子会话**（上面压着另一个 claude.exe）：三态那一路也要走通。
        let child = || Some(HostCapture { pid: 5001, above: Some(4900), start: Some(888) });
        let d2 = tempdir("capture-write-child");
        handle_with(&payload("SessionStart", None, None), &d2, &cfg, 1000, &child).unwrap();
        assert_eq!(state::load_one(&d2, "s1").unwrap().nested, Some(true));

        // 后续事件不抓（`should_capture` 为 false ⇒ 注入的假抓手**根本不会被调用**），
        // 但两个字段必须原样躺在盘上。
        let boom = || panic!("已经有 pid 的会话不该再抓宿主");
        handle_with(&payload("UserPromptSubmit", None, None), &d, &cfg, 1100, &boom).unwrap();
        let s = state::load_one(&d, "s1").unwrap();
        assert_eq!((s.claude_pid, s.claude_start), (Some(4242), Some(777)));
    }

    #[test]
    fn a_renewed_host_replaces_both_halves_of_the_identity() {
        // `SessionStart` 会因为 **resume** 再来一次，那时宿主可能换了进程。
        // 要求：pid 与创建时间**成对更新** —— 绝不许出现"新 pid 配旧创建时间"，
        // 那是这套判据唯一能伤到**真会话**的写法（挂件会把活着的会话判成已退出）。
        let d = tempdir("capture-renew");
        let cfg = Config::default();
        let first = || Some(HostCapture { pid: 4242, above: None, start: Some(777) });
        handle_with(&payload("SessionStart", None, None), &d, &cfg, 1000, &first).unwrap();

        let renewed = || Some(HostCapture { pid: 5555, above: None, start: Some(888) });
        handle_with(&payload("SessionStart", None, None), &d, &cfg, 1200, &renewed).unwrap();

        let s = state::load_one(&d, "s1").unwrap();
        assert_eq!(
            (s.claude_pid, s.claude_start),
            (Some(5555), Some(888)),
            "换了宿主 ⇒ 两半都换成新的（半新半旧会把真会话判死）"
        );

        // 反向：**抓不到**（进程表枚举失败 / 权限不足）⇒ 两半都原样保留。
        let failed = || None;
        handle_with(&payload("SessionStart", None, None), &d, &cfg, 1400, &failed).unwrap();
        let s = state::load_one(&d, "s1").unwrap();
        assert_eq!(
            (s.claude_pid, s.claude_start),
            (Some(5555), Some(888)),
            "抓不到 ⇒ 整组保留，不许清空、也不许只清一半"
        );
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
