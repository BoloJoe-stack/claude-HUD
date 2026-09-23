use crate::config::Config;
use crate::model::{decay, State};
use crate::state::SessionState;
use crate::transcript::Delta;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Tasks { done: u32, total: u32 },
    Steps(u32),
    None,
}

/// 界面一行 = "会名 + 状态 + 计时"（另有第二段的上下文占比 / 子代理，见各字段说明）。
///
/// ⚠️ **界面改造（2026-09-14，用户两轮裁定）**：挂件只显示**工作状态**，不显示具体
/// 内容。下面标了"不渲染"的字段因此**只被赋值、不再有读者** —— `cargo build` 会报
/// "field is never read"，那是**如实反映**，不是待清理的噪音：
///
/// - **不要加 `#[allow(dead_code)]` 掩盖**：那会让"M4 要不要加回来"的决策依据消失。
/// - **也不要删字段**：用户裁定**保留待 M4**。`poll_once` → `poller::refresh` →
///   `parse_delta` 这条解析管线照旧跑（每轮已经付了这份 IO/解析成本，多填几个字段
///   是零边际成本），删掉反而要重写 `view::build_rows_with_deltas` 与一批测试。
///
/// ⚠️ **"不渲染"是逐字段的，不是这一整段的性质。** 第二轮把**上下文占比**加回来了，
/// 所以 `context_pct` **正在渲染**（`ui.rs` 的 `fmt_context` / `context_color`），
/// 它**不在**下面那串"没有读者"的名单里。判断某字段到底有没有读者的**唯一可靠依据
/// 是 `cargo build` 的 never-read 列表**，不是这段注释。同理，M4 现在要恢复的是
/// **当前工具**（`detail`），不是占比。
#[derive(Debug, Clone)]
pub struct Row {
    pub session_id: String,
    pub name: String,
    pub state: State,
    pub elapsed_secs: i64,
    /// **不渲染**（保留待 M4）：当前工具，如 `Bash · cargo test`。解析与拼接照旧。
    pub detail: Option<String>,
    /// **不渲染**（保留待 M4）：上下文 token 数。解析照旧。
    pub context_tokens: Option<u64>,
    /// **不渲染**（保留待 M4）：上下文上限（来自 `Config::context_limit`）。
    pub context_limit: u64,
    /// **渲染中**（第二轮加回，用户要求"显示上下文占比"）：上下文占比 = tokens / limit。
    pub context_pct: Option<f32>,
    /// **不渲染**（保留待 M4）：步数 / 任务进度。解析照旧。
    pub progress: Progress,
    /// **不渲染**（保留待 M4）：终态会话的最后一条助手消息（摘要）。解析照旧。
    pub subtitle: Option<String>,
    /// **在跑的子代理个数**（第二轮新增，用户要求"显示子代理任务情况"）。
    ///
    /// 口径 = 主 transcript 里尚未收到 `tool_result` 的 `Agent` 工具调用数
    /// （推导过程与"为什么不用另外三条信号"见 `transcript::Delta::pending_agents`）。
    /// **没有子代理时恒为 0**，界面据此**不画**这一段（而不是画一个 0）。
    pub subagent_count: u32,
    /// **宿主 Claude Code 进程已经不在了**（9b 的僵尸会话）。
    ///
    /// 由 `ui` 判存活后经 [`mark_host_gone`] 置位 —— **不在本模块里算**，因为判存活要调
    /// Win32（有 syscall），而本模块是纯的、测试全靠喂合成数据。
    ///
    /// 置位时界面显示"已退出"而**不是** `state` 对应的词：这个会话的真实处境是
    /// "它已经不在了"，而不是它最后一个 hook 事件报的那个状态。
    pub host_gone: bool,
}

/// 把"宿主已经不在"的会话标出来，并**把它们沉到列表最后**。
///
/// 单独一个函数而不是塞进 [`build_rows_with_deltas`]，理由见 `Row::host_gone` 的注释：
/// 判定归 `ui`（要 syscall），**排序规则归这里**（纯逻辑）。
///
/// 用**稳定**的 `sort_by_key` 而不是重新算一遍完整排序键：非僵尸行保持
/// `build_rows_with_deltas` 已经排好的顺序，僵尸行整体后移、彼此相对顺序也不变 ——
/// 这样就不必把"最近活动时间"这个排序键的来源再抄一遍（它不在 `Row` 上）。
pub fn mark_host_gone(rows: &mut [Row], gone: &std::collections::HashSet<String>) {
    if gone.is_empty() {
        return;
    }
    for r in rows.iter_mut() {
        r.host_gone = gone.contains(&r.session_id);
    }
    rows.sort_by_key(|r| r.host_gone);
}

/// 排序用的状态分组：**数值越小越靠前**。折叠态聚合与列表排序共用这一个顺序。
///
/// 用户 2026-09-17 裁定的新口径（原话："工作中的会话放在下面，待办或者工作完毕的放在前面…
/// 网络问题或者错误等优先级高于所有…完成的优先级高于待办"）：
///
/// | 组 | 状态 | 为什么在这 |
/// |---|---|---|
/// | 0 | `Error` | "网络问题或者错误等优先级高于所有" —— 出错是唯一需要你**介入修复**的 |
/// | 1 | `Done` / `Interrupted` / `Idle` | **完成组**。用户："完成的优先级高于待办"；"待命"并进来（它本来就是完成降级来的） |
/// | 2 | `Waiting` | **待办**（= 等待确认）：要你动手，但性质是"配合一下"，低于"出事了"和"干完了" |
/// | 3 | `Working` / `Compacting` | 用户："工作中的会话放在下面" |
///
/// ⚠️ 这与 2026-09-14 的首版**正好相反**（那版把 `Waiting` 排第 0、`Done` 排到第 4）。
/// 改的是"谁更该被先看见"，不是 bug 修复 —— 旧注释里"越小越需要你"这句话跟着换成了本表。
fn priority(s: State) -> u8 {
    match s {
        State::Error => 0,
        State::Done => 1,
        State::Interrupted => 1,
        State::Idle => 1,
        State::Waiting => 2,
        State::Working => 3,
        State::Compacting => 3,
    }
}

// 字符串→枚举的转换由 model::State::from_str 提供（Ruling #2：集中定义在
// model.rs，供 Task 6 与 Task 10 共用，不要在此重复实现）。

/// "活动胜过终态"用的余量（秒）。见 `build_rows_with_deltas` 里那条覆盖的说明：
/// 没有它，每个回合结束时最后一行与 `Stop` 的毫秒级先后抖动会让状态闪一下。
const ACTIVITY_SLACK_SECS: i64 = 2;

fn basename(p: &str) -> String {
    p.replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(p)
        .to_string()
}

/// 无 delta 的入口：Task 10 的老签名保持可用，行为与接线前一致（detail / 上下文占比 /
/// 步数一律留空）。真实数据由挂件走 [`build_rows_with_deltas`]。
pub fn build_rows(states: &[SessionState], cfg: &Config, now: i64) -> Vec<Row> {
    build_rows_with_deltas(states, &HashMap::new(), cfg, now)
}

pub fn build_rows_with_deltas(
    states: &[SessionState],
    deltas: &HashMap<String, Delta>,
    cfg: &Config,
    now: i64,
) -> Vec<Row> {
    // 排序键就是 `Row` 自己（`elapsed_secs` = 进入当前状态多久），排完不需要丢弃任何东西。
    let mut keyed: Vec<(Row, i64)> = states
        .iter()
        .map(|s| {
            let delta = deltas.get(&s.session_id);

            let raw = State::from_str(&s.state);
            let state = decay(raw, s.state_since, cfg, now);

            // 子代理还在跑 ⇒ 整体仍是**工作中**。
            //
            // 用户 2026-09-15 实测反馈：子代理在干活、main 已经 `Stop` 在等它的时候，
            // 界面显示的是"完成/待命" —— **与事实相反**（活还在干）。main 的 `Stop`
            // 只说明"主循环把控制权交出去了"，不说明这个会话闲下来了。
            //
            // ⚠️ 只覆盖 `Done` 与 `Idle` 两档：`Waiting`（等你确认）/ `Error` /
            // `Interrupted` 都**更需要你动手**，把它们降级成"工作中"会把真正该看的
            // 那个提示藏掉。（`Interrupted` 紧接着会被下面那段覆盖，顺序无碍。）
            // 前台（还在等回执）+ 后台（已启动、还没收到终止通知）**都算"在跑"**。
            // 分开记是因为两者的完成信号不同，见 `Delta::bg_agents`。
            let subagent_count = delta.map_or(0, |d| {
                (d.pending_agents.len() + d.bg_agents.len()) as u32
            });
            let state = if subagent_count > 0 && matches!(state, State::Done | State::Idle) {
                State::Working
            } else {
                state
            };

            // **活动胜过终态**：转录在"这个状态定下来的时刻"之后又动过 ⇒ 它其实在干活。
            //
            // 用户 2026-09-16 报："明明是在工作中，却显示是等待确认或者工作完成"。
            // 这不是延迟，是**状态机缺两条出路**（`model::transition` 的表里明摆着）——
            // 有两段真实的工作期一个事件都不会发：
            //
            // 1. **权限批了之后**：`Notification(permission_prompt)` → 「等待确认」，而用户点
            //    "允许"**不触发任何已注册事件**；Waiting 的出路只有下一次 `UserPromptSubmit`
            //    / `Stop` / `SessionEnd`，都在回合末尾 ⇒ 从批准到回合结束，界面一直挂着"等待确认"，
            //    人却在看它疯狂干活。
            // 2. **后台子代理干完、主代理被自动唤醒**：那种回合由 `<task-notification>` 驱动，
            //    **没有 `UserPromptSubmit`** ⇒ 上一个 `Stop` 的「完成」会一直挂到回合结束。
            //
            // 转录是唯一能看见"此刻真的在动"的信号：状态文件只在事件边界更新，**转录是流**。
            // 判据用 `last_line_at > state_since`（状态定下来的时刻之后还有新行）。
            //
            // ⚠️ 四处小心：
            // - **只覆盖 `Waiting` / `Done` / `Idle`**。`Error`（API 挂了）与 `Interrupted`
            //   与 `Compacting` 是"要你动手"或"本来就对"的档，被自动顶掉会把该看的提示藏起来。
            // - ⭐ **`Idle` 必须在列**（2026-09-18 补，此前漏了，真机上复发过用户 09-16 报的那句
            //   "明明是在工作中，却显示工作完成"）：`decay` 会在 `idle_after_sec`（默认 300s）
            //   之后把 `Done` **降级成 `Idle`**，而这里判的是**降级之后**的 state ⇒ 上一个
            //   `Stop` 只要过了 5 分钟，"被通知唤醒的回合"就再也顶不上去了。真机场景：
            //   后台子代理跑 > 5 分钟 → 主代理被 `<task-notification>` 唤醒开新回合
            //   （这类回合**不发任何已注册事件**）⇒ 界面上一个正在烧 token 的会话写着"待命"、
            //   计时还在走，排序还把它扔进"完成/待命"组排到前面。
            //   **判据本身（转录有没有动）才是真正的鉴别器**，所以放进 `Idle` 不会误伤：
            //   真空闲的会话 `last_line_at` 就停在 `state_since` 那一带（`Stop` 是读完转录
            //   才发的），过不了下面那道严格大于 + 2s 余量的关 —— 这条由用例里的**反向对照**
            //   ⑦'（转录没再动 ⇒ 老实待命）守着。
            // - **留 2 秒余量**：`Stop` 事件是读完 transcript 才发的（payload 里带着
            //   `last_assistant_message`），所以正常情况下最后一行**早于** `state_since`；
            //   但毫秒级抖动与时钟粒度可能让它们几乎相等，没余量会每回合末闪一下"工作中"。
            // - **真的在等授权时不会误判**：那时主代理是**阻塞**的，转录不写新行，
            //   `last_line_at` 停在发起工具调用那一刻（早于 `state_since`）。
            let transcript_moved_on = delta
                .and_then(|d| d.last_line_at)
                .is_some_and(|t| t > s.state_since + ACTIVITY_SLACK_SECS);
            let state = if transcript_moved_on
                && matches!(state, State::Waiting | State::Done | State::Idle)
            {
                State::Working
            } else {
                state
            };

            // spec §7 第一个坑：Esc 打断不触发任何 hook，只有 transcript 侧的
            // `interruptedMessageId` 能看见它。它是权威的，优先于 hook 推出来的状态。
            //
            // ⚠️ 必须在 `step_count` 之前判：被打断那行的 `content` 是空串 → 不是
            // tool_result 载体 → `parse_delta` 同时把 `step_count` 归零。若让步数参与
            // 判断，"被打断"会被误读成"刚开新一轮"。
            let state = if delta.is_some_and(|d| d.interrupted) {
                State::Interrupted
            } else {
                state
            };

            let name = s
                .display_name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| {
                    let b = basename(&s.cwd);
                    if b.is_empty() {
                        s.session_id.chars().take(8).collect()
                    } else {
                        b
                    }
                });

            // 当前动作行（spec §9）："Bash · cargo test"。
            // 拼接放在这里而不是 `transcript::summarize_tool` 里 —— 后者只认工具名与入参，
            // 不知道界面想怎么显示。
            let detail = delta.and_then(|d| match (&d.last_tool, &d.last_tool_detail) {
                (Some(t), Some(x)) => Some(format!("{} · {}", t, x)),
                (Some(t), None) => Some(t.clone()),
                _ => None,
            });

            // 上下文 token 与占比由 Task 12 的 `build_rows_with_deltas` 从
            // `transcript::Delta` 提供（`poller::refresh` 每轮把 transcript 增量折叠进去）。
            // 没有 delta（会话无 transcript 路径、或文件还没读到）时留空。
            let context_tokens = delta.and_then(|d| d.context_tokens);
            let context_pct = context_tokens
                .map(|t| (t as f64 / cfg.context_limit.max(1) as f64) as f32);

            // spec §2 的"智能回退"：有任务清单时本应显示 n/N，但 spec §4.2 实测
            // TodoWrite / Task 各 0 次，且 §6 的事件集不含 TaskCreated/TaskCompleted，
            // 故当前恒走 Steps 分支。Progress::Tasks 保留定义，等真有数据源再接。
            // 完全没有 delta 时留空（而非 Steps(0)）——"第 0 步"是没有信息量的噪音。
            let progress = match delta {
                Some(d) => Progress::Steps(d.step_count),
                None => Progress::None,
            };

            let subtitle = match state {
                State::Done | State::Error | State::Interrupted => s
                    .last_assistant_message
                    .clone()
                    .or_else(|| s.notification_message.clone()),
                State::Waiting => s.notification_message.clone(),
                _ => None,
            };

            // 在跑的子代理数。没有 delta（会话无 transcript 路径）时恒为 0 ——
            // "读不到"不等于"有子代理在跑"，宁可少报也不凭空多画一段。
            // （它的计算已上移到状态判定那里 —— 状态覆盖要用到它。）

            (
                Row {
                    session_id: s.session_id.clone(),
                    name,
                    state,
                    elapsed_secs: (now - s.state_since).max(0),
                    detail,
                    context_tokens,
                    context_limit: cfg.context_limit,
                    context_pct,
                    progress,
                    subtitle,
                    subagent_count,
                    // `mark_host_gone` 之后才置位（它要 syscall，本模块保持纯函数）。
                    host_gone: false,
                },
                s.last_event_at,
            )
        })
        .collect();

    // 组内**一律"越久越靠前"**（用户 2026-09-17 裁定："越久没有处理的优先级越高"、
    // "工作时间越长的优先级越高"，并且选了"四组统一这一条"）。
    //
    // 判据是 `elapsed_secs` = 进入**当前状态**多久 —— 不是"最近一次活动"（那是旧口径：
    // 同级按 `last_event_at` 降序）。两者在本机常常同向，但语义完全不同：
    // 一条"工作中干了 40 分钟"的会话，最近一次活动是 2 秒前，按旧口径它会排到最后、
    // 按新口径排到最前 —— 用户要的是后者。
    keyed.sort_by(|(a, _), (b, _)| {
        priority(a.state)
            .cmp(&priority(b.state))
            .then_with(|| b.elapsed_secs.cmp(&a.elapsed_secs))
    });

    keyed.into_iter().map(|(row, _)| row).collect()
}

pub fn aggregate(rows: &[Row]) -> Option<State> {
    rows.iter().map(|r| r.state).min_by_key(|s| priority(*s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionState;

    fn st(id: &str, state: &str, since: i64, last_at: i64) -> SessionState {
        SessionState {
            session_id: id.into(),
            cwd: format!("D:\\work\\{}", id),
            transcript_path: None,
            display_name: None,
            claude_pid: None,
            claude_start: None,
            nested: None,
            state: state.into(),
            state_since: since,
            last_event: "X".into(),
            last_event_at: last_at,
            last_assistant_message: None,
            notification_message: None,
            transcript_offset: 0,
        }
    }

    /// **四组顺序**：错误 > 完成组（完成/被打断/待命）> 待办（等待确认）> 工作中（含压缩中）。
    ///
    /// 用户 2026-09-17 裁定。注意这与首版**正好相反**（首版是 等待确认 > 出错 > 工作中 > 完成/待命）。
    #[test]
    fn the_list_order_is_error_then_done_then_waiting_then_working() {
        let states = vec![
            st("working", "working", 0, 10),
            st("waiting", "waiting", 0, 20),
            st("done", "done", 0, 30),
            st("error", "error", 0, 40),
            st("idle", "idle", 0, 50),
            st("interrupted", "interrupted", 0, 60),
            st("compacting", "compacting", 0, 70),
        ];
        let rows = build_rows(&states, &Config::default(), 100);
        let order: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(
            order,
            // 组内同 `state_since` ⇒ 并列，稳定排序保持输入顺序
            vec!["error", "done", "idle", "interrupted", "waiting", "working", "compacting"],
            "错误 > 完成(完成/待命/被打断) > 待办(等待确认) > 工作中(含压缩中)"
        );
    }

    /// **组内一律"越久越靠前"** —— 判据是"进入当前状态多久"，**不是**"最近有没有活动"。
    ///
    /// 用户原话："越久没有处理的优先级越高"、"工作时间越长的优先级越高"，
    /// 并且选了"四组统一这一条"。
    #[test]
    fn within_a_group_the_longest_in_that_state_comes_first() {
        // 两条都在"工作中"，但**两个时间字段故意相反**：
        //   `long`：干了很久（since=0，elapsed=100），但**很久没活动**（last_at=10）
        //   `just`：刚开始（since=90，elapsed=10），但**刚有过活动**（last_at=99）
        //
        // 于是新口径（按"进入状态多久"）排 long 在前，旧口径（按 last_at 降序）排 just 在前
        // —— **夹具必须让两种口径给出相反结果**，否则这条用例测不出任何差别（空断言）。
        let states = vec![st("just", "working", 90, 99), st("long", "working", 0, 10)];
        assert!(
            states[0].last_event_at > states[1].last_event_at,
            "夹具失效：旧口径也会把 long 排前面，这条用例就白写了"
        );
        let rows = build_rows(&states, &Config::default(), 100);
        assert_eq!(
            rows[0].session_id, "long",
            "工作时间越长越靠前（新口径按「进入状态多久」，不是「最近有没有活动」）"
        );
    }

    #[test]
    fn name_falls_back_to_cwd_basename_then_id() {
        let mut named = st("n1", "working", 0, 10);
        named.display_name = Some("示例项目报告排版修复".into());
        let plain = st("n2", "working", 0, 20);

        let rows = build_rows(&[named, plain], &Config::default(), 100);
        let by_id = |id: &str| rows.iter().find(|r| r.session_id == id).unwrap();
        assert_eq!(by_id("n1").name, "示例项目报告排版修复");
        assert_eq!(by_id("n2").name, "n2", "cwd 末段 = n2");
    }

    // 此处原有 context_pct_divides_by_configured_limit 一个用例，**已被 pre-flight
    // 扫描裁定删除**：它先调 build_rows，再手工覆写 rows[0].context_tokens /
    // context_pct，然后断言自己刚赋的值 —— 一行生产代码都没测到（评审规则里的
    // "tests that assert nothing"）。上下文占比的真正覆盖在 Task 12 的
    // delta_supplies_detail_progress_and_context_pct，那里走的是真实数据通路。

    #[test]
    fn done_state_decays_to_idle_after_threshold() {
        let cfg = Config::default(); // idle_after_sec = 300
        let states = vec![st("d1", "done", 1000, 1000)];
        assert_eq!(build_rows(&states, &cfg, 1100)[0].state, State::Done);
        assert_eq!(build_rows(&states, &cfg, 1400)[0].state, State::Idle);
    }

    #[test]
    fn working_never_decays() {
        let cfg = Config::default();
        let states = vec![st("w1", "working", 1000, 1000)];
        assert_eq!(build_rows(&states, &cfg, 999_999)[0].state, State::Working);
    }

    #[test]
    fn elapsed_is_measured_from_state_since() {
        let states = vec![st("e1", "working", 1000, 1000)];
        assert_eq!(build_rows(&states, &Config::default(), 1133)[0].elapsed_secs, 133);
    }

    #[test]
    fn done_row_carries_last_assistant_message_as_subtitle() {
        let mut s = st("s1", "done", 1000, 1000);
        s.last_assistant_message = Some("已修复排版，共 12 处".into());
        let rows = build_rows(&[s], &Config::default(), 1100);
        assert_eq!(rows[0].subtitle.as_deref(), Some("已修复排版，共 12 处"));
    }

    #[test]
    fn aggregate_picks_highest_priority_state() {
        // 与列表排序**共用** `priority`，所以它也随 2026-09-17 的新口径而变化：
        // 完成组（这里用 `idle`）现在高于"待办"（`waiting`）。
        let states = vec![st("a", "idle", 0, 1), st("b", "working", 0, 2), st("c", "waiting", 0, 3)];
        let rows = build_rows(&states, &Config::default(), 100);
        assert_eq!(aggregate(&rows), Some(State::Idle), "完成组 > 待办 > 工作中");
        // 出错仍然压过一切
        let with_error = vec![st("a", "idle", 0, 1), st("b", "error", 0, 2)];
        let rows = build_rows(&with_error, &Config::default(), 100);
        assert_eq!(aggregate(&rows), Some(State::Error));
    }

    #[test]
    fn aggregate_of_empty_is_none() {
        assert_eq!(aggregate(&[]), None);
    }

    #[test]
    fn empty_input_yields_no_rows() {
        assert!(build_rows(&[], &Config::default(), 100).is_empty());
    }
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use crate::config::Config;
    use crate::state::SessionState;
    use crate::transcript::Delta;
    use std::collections::HashMap;

    fn st(id: &str) -> SessionState {
        SessionState {
            session_id: id.into(),
            cwd: "D:\\work\\proj".into(),
            transcript_path: None,
            display_name: Some("样例项目".into()),
            claude_pid: None,
            claude_start: None,
            nested: None,
            state: "working".into(),
            state_since: 0,
            last_event: "X".into(),
            last_event_at: 0,
            last_assistant_message: None,
            notification_message: None,
            transcript_offset: 0,
        }
    }

    #[test]
    fn pending_subagents_keep_a_stopped_session_showing_as_working() {
        // 用户 2026-09-15 实测反馈：子代理在干活、main 已经 `Stop` 在等它的时候，
        // 界面显示的是"完成" —— **与事实相反**（活还在干）。main 的 `Stop` 只说明
        // "主循环把控制权交出去了"，不说明会话闲下来了。
        let cfg = Config::default();
        let mut stopped = st("a");
        stopped.state = "done".into();
        stopped.state_since = 100; // 刚完成、还没到 idle 衰减

        let with_agents = Delta {
            pending_agents: ["call_1".to_string()].into(),
            ..Default::default()
        };
        let rows = build_rows_with_deltas(
            &[stopped.clone()],
            &HashMap::from([("a".to_string(), with_agents)]),
            &cfg,
            100,
        );
        assert_eq!(rows[0].state, State::Working, "有子代理在跑时不能显示完成");

        // **反向对照**：没有子代理时**必须**保持 Done —— 别把这条覆盖写成无条件的，
        // 那会让所有完成态永远显示"工作中"。
        let no_agents = Delta { pending_agents: Default::default(), ..Default::default() };
        let rows = build_rows_with_deltas(
            &[stopped],
            &HashMap::from([("a".to_string(), no_agents)]),
            &cfg,
            100,
        );
        assert_eq!(rows[0].state, State::Done, "没有子代理时该是什么就是什么");
    }

    #[test]
    fn pending_subagents_do_not_downgrade_a_call_for_help() {
        // **只覆盖 `Done` / `Idle` 两档**：`Waiting`（等你确认）与 `Error` 更**需要你
        // 动手**，把它们降级成"工作中"会把真正该看的那个提示藏掉。
        let cfg = Config::default();
        let mut waiting = st("a");
        waiting.state = "waiting".into();
        let with_agents = Delta {
            pending_agents: ["call_1".to_string()].into(),
            ..Default::default()
        };
        let rows = build_rows_with_deltas(
            &[waiting],
            &HashMap::from([("a".to_string(), with_agents)]),
            &cfg,
            100,
        );
        assert_eq!(rows[0].state, State::Waiting, "等你确认不能被降级成工作中");

        // 出错同理
        let mut failed = st("a");
        failed.state = "error".into();
        let rows = build_rows_with_deltas(
            &[failed],
            &HashMap::from([(
                "a".to_string(),
                Delta {
                    pending_agents: ["call_1".to_string()].into(),
                    ..Default::default()
                },
            )]),
            &cfg,
            100,
        );
        assert_eq!(rows[0].state, State::Error, "出错不能被降级成工作中");
    }

    /// **活动胜过终态**：转录在本状态定下来之后又动过 ⇒ 它其实在干活。
    ///
    /// 用户 2026-09-16 报的"明明在工作，却显示等待确认或完成"，根因不是延迟，是两个
    /// **一个 hook 事件都不会发**的工作期（权限批了之后 / 后台子代理干完自动唤醒的回合），
    /// 详见 `build_rows_with_deltas` 里那条覆盖的注释。这组用例把四种组合钉死：
    /// 两个"该被顶成工作中"，两个**反向对照**（不许无条件顶）。
    #[test]
    fn transcript_activity_beats_a_stale_waiting_or_done() {
        let cfg = Config::default();
        let moved = |t: i64| Delta { last_line_at: Some(t), ..Default::default() };
        let idle_transcript = Delta { last_line_at: Some(50), ..Default::default() };

        // ① 等待确认 + 转录在那之后动过（用户刚点了"允许"）⇒ 工作中
        let mut waiting = st("a");
        waiting.state = "waiting".into();
        waiting.state_since = 100;
        let rows = build_rows_with_deltas(
            &[waiting],
            &HashMap::from([("a".to_string(), moved(200))]),
            &cfg,
            300,
        );
        assert_eq!(rows[0].state, State::Working, "批了权限之后就该是工作中");

        // ② 完成 + 转录在那之后动过（后台子代理干完把主代理唤醒了）⇒ 工作中
        let mut done = st("a");
        done.state = "done".into();
        done.state_since = 100;
        let rows = build_rows_with_deltas(
            &[done],
            &HashMap::from([("a".to_string(), moved(200))]),
            &cfg,
            300,
        );
        assert_eq!(rows[0].state, State::Working, "被唤醒的回合也是工作中");

        // ③ **反向对照**：真的在等授权时主代理是阻塞的、转录不写新行 ⇒ 必须保持"等待确认"。
        //    写成无条件覆盖的话，这条会红 —— 那会把"真的在等你"藏成"工作中"。
        let mut waiting = st("a");
        waiting.state = "waiting".into();
        waiting.state_since = 100;
        let rows = build_rows_with_deltas(
            &[waiting],
            &HashMap::from([("a".to_string(), idle_transcript.clone())]),
            &cfg,
            300,
        );
        assert_eq!(rows[0].state, State::Waiting, "转录没动就该老实显示等待确认");

        // ④ **反向对照**：回合正常结束（最后一行早于 Stop）⇒ 必须保持"完成"。
        //    没有这条，每个回合结束时都会闪成"工作中"。
        let mut done = st("a");
        done.state = "done".into();
        done.state_since = 100;
        let rows = build_rows_with_deltas(
            &[done],
            &HashMap::from([("a".to_string(), idle_transcript)]),
            &cfg,
            300,
        );
        assert_eq!(rows[0].state, State::Done, "正常的完成态不许被顶掉");

        // ⑤ `Error` 不在覆盖范围内：API 挂了是要你动手的档，藏掉就看不见了。
        let mut failed = st("a");
        failed.state = "error".into();
        failed.state_since = 100;
        let rows = build_rows_with_deltas(
            &[failed],
            &HashMap::from([("a".to_string(), moved(200))]),
            &cfg,
            300,
        );
        assert_eq!(rows[0].state, State::Error, "出错不许被转录活动顶掉");

        // ⑥ 边界：正好等于 `state_since + 余量` **不算**动过（判据是严格大于）。
        //    这条钉的是"余量"的存在 —— 去掉 `+ ACTIVITY_SLACK_SECS` 它就会红。
        let mut done = st("a");
        done.state = "done".into();
        done.state_since = 100;
        let rows = build_rows_with_deltas(
            &[done],
            &HashMap::from([("a".to_string(), moved(102))]),
            &cfg,
            300,
        );
        assert_eq!(rows[0].state, State::Done, "刚结束那一瞬不该闪成工作中");

        // ⑦ ⭐ **上一个 `Stop` 已经过去 5 分钟以上**（超过 `idle_after_sec`）时，
        //    被唤醒的回合仍必须是"工作中"。
        //
        //    这条是上面②的**同一场景、真实时长**版本，也是这组用例原先唯一的漏洞：
        //    ②③④⑤⑥ 的夹具全是 `state_since=100 / now=300`（相隔 200s < 阈值 300s），
        //    **一次都没跨过 `decay`**。而真机上"后台子代理跑超过 5 分钟"是常事 ——
        //    那时 `decay` 先把 `done@T0` 降级成 `Idle`，而覆盖只认 `Waiting | Done`
        //    ⇒ **打不上**，界面上是一个正在烧 token 的会话写着"待命"，计时还在走，
        //    排序还把它扔进"完成/待命"组排到前面（用户 09-17 拍板"工作中的放下边"）。
        //    用户 09-16 报的那句"明明是在工作中，却显示工作完成"在这里**原样复发**。
        let mut done = st("a");
        done.state = "done".into();
        done.state_since = 100;
        let rows = build_rows_with_deltas(
            &[done],
            &HashMap::from([("a".to_string(), moved(400))]),
            &cfg,
            500, // ← now: 距 state_since 400s > idle_after_sec(300) ⇒ 已经 decay 成 Idle
        );
        assert_eq!(
            rows[0].state,
            State::Working,
            "被唤醒的回合哪怕上一个 Stop 已过 5 分钟，也还是工作中"
        );

        // ⑦' **反向对照**：同一个"早就 Stop 过"的会话，转录**没有**再动 ⇒ 必须老实待命。
        //     没有这一条，"把所有 Idle 一律顶成工作中"这种实现也能过 ——
        //     而那样每个空闲会话都会永远显示"工作中"。
        let mut done = st("a");
        done.state = "done".into();
        done.state_since = 100;
        let rows = build_rows_with_deltas(
            &[done],
            &HashMap::from([("a".to_string(), Delta { last_line_at: Some(98), ..Default::default() })]),
            &cfg,
            500,
        );
        assert_eq!(rows[0].state, State::Idle, "转录没再动，就该按 decay 老实显示待命");
    }

    #[test]
    fn delta_supplies_detail_progress_and_context_pct() {
        let cfg = Config::default(); // limit = 1_000_000
        let mut deltas = HashMap::new();
        deltas.insert(
            "a".to_string(),
            Delta {
                context_tokens: Some(363_026),
                ai_title: None,
                last_tool: Some("Bash".into()),
                last_tool_detail: Some("cargo test".into()),
                interrupted: false,
                step_count: 12,
                pending_agents: ["call_1".to_string(), "call_2".to_string()].into(),
                // 会话名那三档与本用例无关，用 `..Default::default()` 免得每次加字段
                // 都要回来补（本文件里其余的 `Delta` 构造早已是这个写法）。
                ..Default::default()
            },
        );

        let rows = build_rows_with_deltas(&[st("a")], &deltas, &cfg, 100);
        let r = &rows[0];
        assert_eq!(r.detail.as_deref(), Some("Bash · cargo test"));
        assert_eq!(r.context_tokens, Some(363_026));
        assert!((r.context_pct.unwrap() - 0.363026).abs() < 1e-6);
        assert_eq!(r.progress, Progress::Steps(12));
        assert_eq!(r.subagent_count, 2, "两个未配对的 Agent 调用 = 两个在跑的子代理");
    }

    #[test]
    fn session_without_delta_degrades_gracefully() {
        let cfg = Config::default();
        let rows = build_rows_with_deltas(&[st("a")], &HashMap::new(), &cfg, 100);
        assert!(rows[0].detail.is_none());
        assert!(rows[0].context_pct.is_none());
        assert_eq!(rows[0].progress, Progress::None);
        // 读不到 transcript ≠ 有子代理在跑：这条必须钉住，否则"没子代理却显示
        // 子代理 0/1"这类误报会从 view 层漏进界面。
        assert_eq!(rows[0].subagent_count, 0);
    }

    #[test]
    fn delta_with_no_pending_agents_yields_zero() {
        // 有 delta、但里面没有任何未配对的 Agent（就是"没有子代理"的日常状态）。
        let cfg = Config::default();
        let mut deltas = HashMap::new();
        deltas.insert(
            "a".to_string(),
            Delta { context_tokens: Some(1000), step_count: 3, ..Default::default() },
        );
        let rows = build_rows_with_deltas(&[st("a")], &deltas, &cfg, 100);
        assert_eq!(rows[0].subagent_count, 0);
    }

    #[test]
    fn interrupted_delta_beats_zero_step_count() {
        // 账本 #98：interrupted 的优先级在 view 层没有任何测试。真实数据里
        // interrupted 总是伴随 `step_count == 0`（被打断那行的 content 是空串 →
        // 不算 tool_result 载体 → `parse_delta` 同时把步数归零），所以这正是
        // "让步数参与判断"的写法会走错的那个输入：它会把"被打断"误读成"刚开新一轮"。
        let cfg = Config::default();
        let mut deltas = HashMap::new();
        deltas.insert(
            "a".to_string(),
            Delta {
                interrupted: true,
                step_count: 0,
                ..Default::default()
            },
        );
        let rows = build_rows_with_deltas(&[st("a")], &deltas, &cfg, 100);
        assert_eq!(rows[0].state, State::Interrupted, "打断是权威信号，优先于步数");
        assert_eq!(rows[0].progress, Progress::Steps(0), "步数照常为 0，不影响状态判定");
    }

    #[test]
    fn build_rows_still_works_as_before() {
        // Task 10 的老签名必须保持可用，而且必须与**委托目标的行为逐字段等价**：
        // 只断言 `len() == 1` 是用例账本点名的那种"只测签名可用性、一行生产代码都没
        // 测到"的写法 —— 委托时传错参数（例如传了非空 map）、或将来两边分叉都不会
        // 被发现。取两个状态不同的会话，让排序、subtitle 分支也被覆盖到。
        let cfg = Config::default();

        let mut waiting = st("a");
        waiting.state = "waiting".into();
        waiting.last_event_at = 10;
        waiting.notification_message = Some("需要授权".into());
        let mut done = st("b");
        done.state = "done".into();
        done.last_event_at = 20;
        done.last_assistant_message = Some("已修复排版，共 12 处".into());
        let states = vec![waiting, done];

        let via_old = build_rows(&states, &cfg, 100);
        let via_new = build_rows_with_deltas(&states, &HashMap::new(), &cfg, 100);

        assert_eq!(via_old.len(), 2, "老签名必须保持可用");
        assert_eq!(via_old.len(), via_new.len());
        for (a, b) in via_old.iter().zip(via_new.iter()) {
            assert_eq!(a.session_id, b.session_id);
            assert_eq!(a.name, b.name);
            assert_eq!(a.state, b.state);
            assert_eq!(a.elapsed_secs, b.elapsed_secs);
            assert_eq!(a.detail, b.detail);
            assert_eq!(a.context_tokens, b.context_tokens);
            assert_eq!(a.context_limit, b.context_limit);
            assert_eq!(a.context_pct, b.context_pct);
            assert_eq!(a.progress, b.progress);
            assert_eq!(a.subtitle, b.subtitle);
            assert_eq!(a.subagent_count, b.subagent_count);
        }
        // 顺带：老签名走的是"无 delta"通路，所以 detail/占比/步数一律留空
        assert!(via_old.iter().all(|r| r.detail.is_none() && r.progress == Progress::None));
    }
}
