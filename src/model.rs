use crate::config::Config;
use crate::hook::HookPayload;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Waiting,
    Compacting,
    Done,
    Error,
    Interrupted,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Waiting => "waiting",
            State::Compacting => "compacting",
            State::Done => "done",
            State::Error => "error",
            State::Interrupted => "interrupted",
        }
    }

    /// 与 `as_str` 配对的反向转换。未知状态串回落 `Idle` —— 状态文件是跨进程
    /// 契约，读到未来版本写入的未知值时不能报错。
    pub fn from_str(s: &str) -> State {
        match s {
            "working" => State::Working,
            "waiting" => State::Waiting,
            "compacting" => State::Compacting,
            "done" => State::Done,
            "error" => State::Error,
            "interrupted" => State::Interrupted,
            _ => State::Idle,
        }
    }
}

pub struct Transition {
    pub state: State,
    pub delete: bool,
}

pub fn transition(current: State, p: &HookPayload, _cfg: &Config, _now: i64) -> Transition {
    let keep = |s: State| Transition { state: s, delete: false };

    match p.hook_event_name.as_str() {
        "SessionStart" => keep(State::Idle),
        "UserPromptSubmit" => keep(State::Working),
        "Notification" => {
            // 只有 permission_prompt 代表"在等你"，其他通知类型不改变状态
            if p.notification_type.as_deref() == Some("permission_prompt") {
                keep(State::Waiting)
            } else {
                keep(current)
            }
        }
        "PreCompact" => keep(State::Compacting),
        "PostCompact" => keep(State::Working),
        "Stop" => keep(State::Done),
        "StopFailure" => keep(State::Error),
        "SessionEnd" => Transition { state: State::Idle, delete: true },
        _ => keep(current),
    }
}

// Interrupted 目前没有事件能产生它——它由 Task 9 的 transcript 解析置位。
// 此处先让它参与 decay，Task 9 再补上赋值路径。
/// 只有终态会因久无活动而降级为"待命"；进行中的状态不会。
pub fn decay(state: State, state_since: i64, cfg: &Config, now: i64) -> State {
    match state {
        State::Done | State::Error | State::Interrupted => {
            if cfg.idle_after_sec > 0 && now - state_since >= cfg.idle_after_sec as i64 {
                State::Idle
            } else {
                state
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::HookPayload;

    fn payload(event: &str, notif_type: Option<&str>) -> HookPayload {
        HookPayload {
            session_id: "s".into(),
            transcript_path: None,
            cwd: "D:\\w".into(),
            hook_event_name: event.into(),
            permission_mode: None,
            message: None,
            title: None,
            notification_type: notif_type.map(|s| s.to_string()),
            last_assistant_message: None,
            source: None,
        }
    }

    fn go(cur: State, event: &str, notif: Option<&str>) -> Transition {
        transition(cur, &payload(event, notif), &Config::default(), 1000)
    }

    #[test]
    fn session_start_is_idle() {
        assert_eq!(go(State::Done, "SessionStart", None).state, State::Idle);
    }

    #[test]
    fn user_prompt_submit_starts_working() {
        // spec §7：这是"开始工作"的唯一可靠信号
        assert_eq!(go(State::Idle, "UserPromptSubmit", None).state, State::Working);
        assert_eq!(go(State::Done, "UserPromptSubmit", None).state, State::Working);
    }

    #[test]
    fn permission_notification_means_waiting() {
        let t = go(State::Working, "Notification", Some("permission_prompt"));
        assert_eq!(t.state, State::Waiting);
    }

    #[test]
    fn non_permission_notification_does_not_hijack_state() {
        // 只有 permission_prompt 才代表"等待确认"
        let t = go(State::Working, "Notification", Some("idle_prompt"));
        assert_eq!(t.state, State::Working);
    }

    #[test]
    fn precompact_enters_compacting_and_postcompact_returns_to_working() {
        assert_eq!(go(State::Working, "PreCompact", None).state, State::Compacting);
        assert_eq!(go(State::Compacting, "PostCompact", None).state, State::Working);
    }

    #[test]
    fn stop_means_done() {
        assert_eq!(go(State::Working, "Stop", None).state, State::Done);
    }

    #[test]
    fn stop_failure_means_error() {
        assert_eq!(go(State::Working, "StopFailure", None).state, State::Error);
    }

    #[test]
    fn session_end_marks_for_deletion() {
        let t = go(State::Working, "SessionEnd", None);
        assert!(t.delete, "SessionEnd 必须删除状态文件");
        // 返回值里的 `state` 此前无断言（账本 #24）。它的语义是"文件不存在时的兜底
        // 值"（spec §8：状态枚举只描述"还活着"的会话，SessionEnd 直接删文件），
        // 但只钉 `delete` 的话，把这里改成 Working 之类的变异不会被任何用例发现。
        assert_eq!(t.state, State::Idle);
    }

    #[test]
    fn no_event_other_than_session_end_marks_for_deletion() {
        // 账本 #25：原先全文**没有一处**断言 `delete` 为假 —— 一个把每个分支都设成
        // `delete: true` 的变异体能通过当时全部 11 个测试，而它会在**每一个** hook
        // 事件上删光所有会话状态文件（挂件永远空白，且用户看不到任何错误）。
        // 逐事件钉住"只有 SessionEnd 删"，含 Notification 的两个分支与未知事件的
        // catch-all 分支。变异已验证：把 `keep` 的 delete 改成 true，本用例失败，
        // 而其余 106 个用例照旧通过。
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "Notification",
            "PreCompact",
            "PostCompact",
            "Stop",
            "StopFailure",
            "SomethingNew",
        ] {
            let t = go(State::Working, event, None);
            assert!(!t.delete, "{event} 不得删除状态文件");
        }
        // Notification 的另一分支（permission_prompt）同样不得删
        assert!(!go(State::Working, "Notification", Some("permission_prompt")).delete);
    }

    #[test]
    fn unknown_event_leaves_state_untouched() {
        assert_eq!(go(State::Working, "SomethingNew", None).state, State::Working);
    }

    #[test]
    fn done_decays_to_idle_after_threshold() {
        let cfg = Config::default(); // idle_after_sec = 300
        assert_eq!(decay(State::Done, 1000, &cfg, 1000 + 299), State::Done);
        assert_eq!(decay(State::Done, 1000, &cfg, 1000 + 301), State::Idle);
    }

    #[test]
    fn working_does_not_decay() {
        // 正在干活不能因为"久未更新"就显示成待命
        let cfg = Config::default();
        assert_eq!(decay(State::Working, 1000, &cfg, 999_999), State::Working);
    }

    #[test]
    fn zero_idle_after_sec_means_never_decay() {
        // 账本 #23：`idle_after_sec == 0` 的守卫（语义"0 = 永不降级"）此前无测试。
        // 没有它的变异就是 `now - state_since >= 0` 恒真 —— 三个终态会在下一个事件
        // 到来之前**立刻**塌成待命，用户看到"刚完成"一闪而过。
        let mut cfg = Config::default();
        cfg.idle_after_sec = 0;
        for state in [State::Done, State::Error, State::Interrupted] {
            assert_eq!(
                decay(state, 1000, &cfg, 1_000_000),
                state,
                "idle_after_sec = 0 时 {} 永不降级",
                state.as_str()
            );
        }
    }

    #[test]
    fn from_str_is_the_inverse_of_as_str_for_every_variant() {
        // Ruling #2 让这个转换成为 T6/T10 的共用契约；它是从持久化状态读回来的唯一入口，
        // 映射错了界面就会直接显示错状态。遍历全部变体，任何一处对调都会让往返失败。
        for v in [
            State::Idle, State::Working, State::Waiting, State::Compacting,
            State::Done, State::Error, State::Interrupted,
        ] {
            assert_eq!(State::from_str(v.as_str()), v, "{} 的往返不成立", v.as_str());
        }
    }

    #[test]
    fn from_str_falls_back_to_idle_for_unknown_status() {
        // 状态文件是跨进程契约：读到未来版本写入的未知状态串不能报错
        assert_eq!(State::from_str("something-new"), State::Idle);
        assert_eq!(State::from_str(""), State::Idle);
    }
}
