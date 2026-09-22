//! `claude-hud --doctor`：拿**真机数据**核一遍那些"靠约定、靠措辞、靠路径"的判据还成不成立。
//!
//! ## 为什么需要它（这是本项目最贵的一课换来的）
//!
//! 2026-09-16 用户报"任务结束了还显示工作中"。根因是**两个判据在真实数据上一次都没命中过**：
//! 后台子代理的完成通知其实长在 `queue-operation` 行的顶层字符串上，而代码只在 `user` 行里找它
//! —— 测试却全绿，因为**夹具是我编的、不是真机上取样的**。这类失效的共同特征：
//! **不报错、不崩、数字看起来完全合理**，只有拿真实数据去量才看得见。
//!
//! 所以本模块只做一件事：**把那些判据拿到最近的真实转录/配置上重新量一遍**，
//! 然后明确告诉你"还成立 / 已经不成立了 / 没法核（最近没这类数据）"。
//!
//! ## 边界
//!
//! - **只读**：不写任何文件（除了调用方弹的那个对话框）。
//! - **零网络**：连中转站都不碰（那是另一回事，见 `docs/工作日志.md`）。
//! - **不猜**：核不了就写"无法核"，并说清为什么 —— 报个假绿比不报更坏。

use std::path::{Path, PathBuf};

use serde_json::Value;

/// 一条检查的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Level {
    Ok,
    /// 还能用，但有值得看一眼的地方（例如"最近没有这类数据，所以核不了"）。
    Warn,
    /// 判据已经不成立 —— 对应功能现在是坏的，或者随时会坏。
    Bad,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    /// 一句话结论（谁看都懂）。
    pub title: String,
    /// 依据：量到了什么、样本多大。**必须带数字**，否则读者没法判断可信度。
    pub detail: String,
}

impl Finding {
    fn ok(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { level: Level::Ok, title: title.into(), detail: detail.into() }
    }
    fn warn(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { level: Level::Warn, title: title.into(), detail: detail.into() }
    }
    fn bad(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { level: Level::Bad, title: title.into(), detail: detail.into() }
    }
}

/// 把结论渲染成一段给人看的纯文本（`show_message` 用）。
///
/// **有问题的排前面**，然后是"核不了"，最后是不成问题的 —— 人打开这个框是为了看"有没有事"。
pub fn render(fs: &[Finding]) -> String {
    let mut order: Vec<&Finding> = Vec::with_capacity(fs.len());
    for want in [Level::Bad, Level::Warn, Level::Ok] {
        order.extend(fs.iter().filter(|f| f.level == want));
    }
    let icon = |l: &Level| match l {
        Level::Ok => "OK  ",
        Level::Warn => "注意",
        Level::Bad => "失效",
    };
    let mut out = String::new();
    let bad = fs.iter().filter(|f| f.level == Level::Bad).count();
    let warn = fs.iter().filter(|f| f.level == Level::Warn).count();
    out.push_str(&format!(
        "共 {} 项检查：{} 项失效、{} 项要注意、{} 项正常。\n\n",
        fs.len(),
        bad,
        warn,
        fs.len() - bad - warn
    ));
    for f in order {
        out.push_str(&format!("[{}] {}\n      {}\n", icon(&f.level), f.title, f.detail));
    }
    out
}

/// 核不了时统一用这句 —— 说明**为什么**核不了，别让人以为"通过"了。
fn cannot_check(why: &str) -> Finding {
    Finding::warn("无法核（样本里没有这类数据）", why)
}

// ---- 逐项检查（能取到文本的都做成纯函数，便于单测）----------------------------

/// ① **hook 注册**：8 个事件是否都指向一个**真实存在**的 exe。
///
/// 这是最要命的一条：注册指向的是**构建产物路径**（`target\release\claude-hud.exe`），
/// 一次 `cargo clean` 或换目录，8 个 hook 会**静默失效** —— Claude Code 照常跑，
/// 挂件一个会话都不显示，也不报错。
pub fn check_hooks(settings_path: &Path) -> Finding {
    let Ok(raw) = std::fs::read_to_string(settings_path) else {
        return Finding::bad(
            "hook 未注册（读不到 settings.json）",
            format!("路径：{}", settings_path.display()),
        );
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return Finding::bad(
            "hook 未注册（settings.json 不是合法 JSON）",
            format!("路径：{}", settings_path.display()),
        );
    };
    let hooks = v.get("hooks");
    let mut missing: Vec<&str> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    for ev in crate::hooks_install::EVENTS {
        let cmds: Vec<String> = hooks
            .and_then(|h| h.get(ev))
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.get("hooks").and_then(|h| h.as_array()))
                    .flatten()
                    .filter_map(|h| h.get("command").and_then(|c| c.as_str()))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if cmds.is_empty() {
            missing.push(ev);
        } else {
            paths.extend(cmds);
        }
    }
    if !missing.is_empty() {
        return Finding::bad(
            format!("hook 缺 {} 个事件", missing.len()),
            format!("缺：{}", missing.join(", ")),
        );
    }
    // 事件齐了 ⇒ 再看那些 exe 还在不在（去重，8 个事件通常指向同一个文件）
    paths.sort();
    paths.dedup();
    let gone: Vec<&String> = paths.iter().filter(|p| !Path::new(p.as_str()).exists()).collect();
    if gone.is_empty() {
        Finding::ok(
            "hook 注册完整，且指向的 exe 存在",
            format!("8 个事件 → {}", paths.join(" · ")),
        )
    } else {
        Finding::bad(
            "hook 指向的 exe 不存在 —— 8 个事件都会静默失效",
            format!("找不到：{}", gone.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" · ")),
        )
    }
}

/// ② **完成通知的形态**：`<task-notification>` 是不是仍然长在"顶层字符串 `content`"上。
///
/// 这条一旦变形，`bg_agents` 就会只增不减 ⇒ 主代理 `Stop` 之后**永远显示"工作中"**
/// （2026-09-16 用户的报障）。
pub fn check_notification_shape(lines: &[String]) -> Finding {
    let mut on_carrier = 0usize;
    let mut elsewhere = 0usize;
    for line in lines {
        if !line.contains("<task-notification>") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // ⚠️ **先滤掉"承载内容的三类行"，再判形态** —— 顺序不能反。
        //
        // `user` / `assistant` / `attachment` 里出现这串 XML，几乎必然是**有人在引用它**：
        // 我自己 grep 源码、读 `transcript.rs`、写测试夹具时，那串字都会作为工具输出进转录。
        // 把引用算成"形态变了"就是**测量污染自己** —— 这条检查第一版正是这么误报的
        // （真机 40 份转录里 173 条"不认得"，逐条看下去全是这三类的引用；
        // 真通知 39 条另外躺在顶层字符串上）。
        //
        // 过滤必须在**分类之前**：否则一条"顶层字符串 content 里带着这串字"的 attachment
        // （看着像通知、其实是引用）会被算成"认得的通知"，把计数注水。
        // 产品侧的同一条纪律见 `transcript::is_notification_carrier` 的注释。
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if matches!(ty, "user" | "assistant" | "attachment") {
            continue;
        }
        if v.get("content").and_then(|c| c.as_str()).is_some_and(|s| s.contains("<task-notification>")) {
            on_carrier += 1;
        } else {
            elsewhere += 1;
        }
    }
    if on_carrier + elsewhere == 0 {
        return cannot_check("最近这些转录里一条后台子代理完成通知都没有，没法验证形态");
    }
    if elsewhere == 0 {
        Finding::ok(
            format!("完成通知形态未变（{on_carrier} 条）"),
            "全部落在顶层字符串 content 上（= 代码认的那种）".to_string(),
        )
    } else {
        Finding::bad(
            format!("完成通知换了形态：{elsewhere} 条不在顶层 content 上"),
            format!("认得的 {on_carrier} 条 · 不认得的 {elsewhere} 条 —— 后台子代理会永远显示「工作中」"),
        )
    }
}

/// ③ **启动回执的措辞**：那句"已在后台启动"是不是还带那两个 marker。
///
/// 这条是**纯文本匹配**（该块没有结构性标志），措辞一改就静默失效 ——
/// 表现是"后台代理一启动就被当成跑完"。判据本身没法加强，只能靠这条检查提前发现。
pub fn check_launch_ack_wording(lines: &[String]) -> Finding {
    const MARKERS: [&str; 2] = [
        "Async agent launched successfully",
        "The agent is working in the background",
    ];
    let mut acknowledged = 0usize; // 带 marker 的 tool_result 行
    let mut suspicious = 0usize; // 像回执（提到 agentId / internal ID）却没有 marker 的
    for line in lines {
        let has_any = MARKERS.iter().any(|m| line.contains(m));
        if has_any {
            acknowledged += 1;
        } else if line.contains("agentId") && line.contains("internal ID") {
            suspicious += 1;
        }
    }
    if acknowledged == 0 && suspicious == 0 {
        return cannot_check("最近这些转录里没有后台子代理的启动回执，没法验证措辞");
    }
    if suspicious == 0 {
        Finding::ok(
            format!("启动回执措辞未变（{acknowledged} 条命中）"),
            "两句 marker 仍在".to_string(),
        )
    } else {
        Finding::bad(
            format!("启动回执措辞可能改了：{suspicious} 条像回执但不带 marker"),
            format!("命中 marker 的 {acknowledged} 条 · 疑似变形的 {suspicious} 条"),
        )
    }
}

/// ④ **usage 行的可解析率**：`uuid` / `timestamp` / `usage` / `message.id` 的命中率。
///
/// 前三个是我们算 token 的**必需**字段（缺一个就丢一行），第四个是归组键
/// （缺了会退回 uuid，那会让同一次响应的多行各算一遍 —— 实测虚高 2.29 倍）。
pub fn check_usage_fields(lines: &[String]) -> Finding {
    let (mut assistants, mut uuid, mut ts, mut usage, mut msg_id) = (0usize, 0, 0, 0, 0);
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        assistants += 1;
        if v.get("uuid").and_then(|x| x.as_str()).is_some() {
            uuid += 1;
        }
        if v.get("timestamp")
            .and_then(|x| x.as_str())
            .and_then(crate::transcript::parse_ts)
            .is_some()
        {
            ts += 1;
        }
        let msg = v.get("message");
        if msg.and_then(|m| m.get("usage")).is_some() {
            usage += 1;
        }
        if msg.and_then(|m| m.get("id")).and_then(|i| x_str(i)).is_some() {
            msg_id += 1;
        }
    }
    if assistants == 0 {
        return cannot_check("最近这些转录里没有 assistant 行，没法核字段命中率");
    }
    let pct = |n: usize| (n as f64) * 100.0 / (assistants as f64);
    let detail = format!(
        "{assistants} 行 assistant：uuid {:.1}% · timestamp {:.1}% · usage {:.1}% · message.id {:.1}%",
        pct(uuid),
        pct(ts),
        pct(usage),
        pct(msg_id)
    );
    // 阈值刻意宽松：真机本来就允许少量坏行（转录被并发截断）。
    if pct(uuid) < 90.0 || pct(ts) < 90.0 || pct(usage) < 90.0 {
        Finding::bad("usage 行的必需字段命中率过低", detail)
    } else if pct(msg_id) < 90.0 {
        Finding::warn(
            "message.id 命中率偏低 —— token 会重复计算",
            format!("{detail}（缺 message.id 的行只能退回 uuid 去重）"),
        )
    } else {
        Finding::ok("usage 行字段齐全", detail)
    }
}

fn x_str(v: &Value) -> Option<&str> {
    v.as_str()
}

/// ⑤ **hook 到底还写不写状态文件**（挂件的数据源就是它）。
pub fn check_state_files(sessions_dir: &Path) -> Finding {
    let Ok(rd) = std::fs::read_dir(sessions_dir) else {
        return Finding::bad(
            "状态目录不存在 —— hook 从来没成功写过东西",
            format!("路径：{}", sessions_dir.display()),
        );
    };
    let mut newest: Option<(std::time::SystemTime, String)> = None;
    let mut count = 0usize;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        count += 1;
        if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
            if newest.as_ref().is_none_or(|(t, _)| m > *t) {
                newest = Some((m, p.file_name().unwrap_or_default().to_string_lossy().to_string()));
            }
        }
    }
    let Some((t, name)) = newest else {
        return cannot_check("状态目录是空的 —— 没有会话用过它，也就没法判断 hook 是否在写");
    };
    let age = std::time::SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let detail = format!("{count} 个状态文件，最近一个是 {name}（{age} 秒前）");
    if age > 24 * 3600 {
        Finding::warn(
            "最近 24 小时没有任何 hook 写过状态文件",
            format!("{detail} —— 除非这期间你确实没用 Claude Code，否则 hook 可能没在跑"),
        )
    } else {
        Finding::ok("hook 在写状态文件", detail)
    }
}

/// ⑥ **今日 token 账本**（顺便把现在这个数报出来，省得再开一次挂件看）。
pub fn check_today_tokens(projects_dir: &Path) -> Finding {
    let mut sc = crate::usage::Scanner::new(projects_dir);
    sc.scan(crate::usage::today());
    let l = sc.ledger();
    if l.day().is_none() {
        return cannot_check(&format!("{} 下没有扫到今天的转录", projects_dir.display()));
    }
    Finding::ok(
        format!("今日 token：{}", crate::usage::fmt_tokens(l.total)),
        format!(
            "{} 条会话 · {} 个项目（按 message.id 归组后的口径）",
            l.by_session.len(),
            l.by_project.len()
        ),
    )
}

// ---- 取样与入口 --------------------------------------------------------------

/// 取最近改动过的若干份转录的行（最新优先，取够 `max_files` 份就停）。
///
/// **只看最近动过的**：老转录里的形态可能早就变了，用它去核"现在还行不行"没有意义，
/// 反而会把已经修好的问题报出来。
pub fn recent_transcript_lines(projects_dir: &Path, max_files: usize) -> Vec<String> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let Ok(projects) = std::fs::read_dir(projects_dir) else {
        return Vec::new();
    };
    for proj in projects.flatten() {
        let Ok(sub) = std::fs::read_dir(proj.path()) else {
            continue;
        };
        for entry in sub.flatten() {
            let p = entry.path();
            let mut push = |path: PathBuf| {
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && let Ok(m) = std::fs::metadata(&path).and_then(|m| m.modified())
                {
                    files.push((m, path));
                }
            };
            if p.is_dir() {
                // 子代理转录（`<session>/subagents/*.jsonl`）：启动回执与通知的样本
                // 有一大半在这里，不能漏
                if let Ok(subs) = std::fs::read_dir(p.join("subagents")) {
                    for f in subs.flatten() {
                        push(f.path());
                    }
                }
            } else {
                push(p);
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files
        .into_iter()
        .take(max_files)
        .flat_map(|(_, p)| {
            std::fs::read_to_string(p)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// 跑全部检查。样本取最近 40 份转录（含子代理）—— 再多就慢了，再少可能取样不到通知。
pub fn run(sample_files: usize) -> Vec<Finding> {
    let lines = recent_transcript_lines(&crate::paths::real_projects_dir(), sample_files);
    vec![
        check_hooks(&crate::hooks_install::default_settings_path()),
        check_state_files(&crate::paths::real_sessions_dir()),
        check_notification_shape(&lines),
        check_launch_ack_wording(&lines),
        check_usage_fields(&lines),
        check_today_tokens(&crate::paths::real_projects_dir()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一行**真形态**的通知（顶层字符串 content、type 是 queue-operation）。
    fn notif_line(tool_use_id: &str) -> String {
        serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": format!("<task-notification>\n<tool-use-id>{tool_use_id}</tool-use-id>\n<status>completed</status>\n</task-notification>")
        })
        .to_string()
    }

    #[test]
    fn notification_shape_holds_on_the_real_form() {
        let f = check_notification_shape(&[notif_line("call_1")]);
        assert_eq!(f.level, Level::Ok, "真形态必须判 OK：{f:?}");
    }

    /// **反向对照**：我自己在会话里引用那串 XML（写源码、贴日志、grep 结果）**不算通知变形**。
    ///
    /// 第一版漏了这条，于是在真机上误报 173 条 —— 全是 `assistant` / `user` / `attachment`
    /// 里的引用。**测量污染自己的语料**是本仓的老坑，这次它反过来咬了检查工具本身。
    #[test]
    fn our_own_echoes_of_the_notification_xml_are_not_mistaken_for_a_new_shape() {
        let echo_in_my_text = serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "<task-notification>
<tool-use-id>call_x</tool-use-id>"}]}
        })
        .to_string();
        let echo_in_tool_output = serde_json::json!({
            "type": "user",
            "message": {"content": [{"type": "tool_result", "content": "<task-notification>…</task-notification>"}]}
        })
        .to_string();
        let echo_in_attachment = serde_json::json!({
            "type": "attachment",
            "content": "…grep 到的片段：<task-notification>"
        })
        .to_string();
        let fs = check_notification_shape(&[
            echo_in_my_text,
            echo_in_tool_output,
            echo_in_attachment,
            notif_line("call_1"),
        ]);
        assert_eq!(fs.level, Level::Ok, "引用不是变形：{fs:?}");
        assert!(fs.title.contains("1 条"), "只该把真通知算进来：{fs:?}");
    }

    #[test]
    fn notification_shape_is_flagged_when_it_moves_elsewhere() {
        // 变异体：通知换成了**系统注入行的另一种形态** —— `content` 不再是顶层字符串，
        // 而是藏在别的字段里（真出现这种情况，我们的判据就认不出来了）。
        // ⚠️ 夹具**不能写成 `user` 行**：那属于"有人引用这串字"，本条判据有意忽略它
        // （见 `our_own_echoes_...` 的反向对照）。
        let moved = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "payload": {"text": "<task-notification>\n<tool-use-id>call_1</tool-use-id>\n</task-notification>"}
        })
        .to_string();
        let f = check_notification_shape(&[moved]);
        assert_eq!(f.level, Level::Bad, "形态变了必须报失效：{f:?}");
    }

    #[test]
    fn no_samples_is_reported_as_cannot_check_not_as_ok() {
        // ⭐ 这条钉的是"不猜"：样本里没有通知时**不许**报 OK —— 报假绿比不报更坏
        let f = check_notification_shape(&["{\"type\":\"assistant\"}".to_string()]);
        assert_eq!(f.level, Level::Warn);
        assert!(f.title.contains("无法核"), "{f:?}");
    }

    #[test]
    fn launch_ack_wording_is_flagged_when_the_markers_disappear() {
        let good = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully. agentId: a2a4 (internal ID)"}]}]}}"#.to_string();
        assert_eq!(check_launch_ack_wording(&[good]).level, Level::Ok);

        // 措辞改了：还提 agentId，但那两句 marker 都不在
        let changed = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":[{"type":"text","text":"Background worker started. agentId: a2a4 (internal ID)"}]}]}}"#.to_string();
        let f = check_launch_ack_wording(&[changed]);
        assert_eq!(f.level, Level::Bad, "措辞变了必须报：{f:?}");
    }

    #[test]
    fn usage_field_hit_rates_are_measured_and_low_rates_are_flagged() {
        let day = "2026-09-16T02:00:00.000Z";
        let good = serde_json::json!({
            "type": "assistant", "uuid": "u", "timestamp": day,
            "message": {"id": "m", "usage": {"input_tokens": 1}}
        })
        .to_string();
        assert_eq!(check_usage_fields(&[good.clone()]).level, Level::Ok);

        // 缺 uuid / 时间戳 / usage 的行：命中率被拉到 0 ⇒ 报失效
        let bad = serde_json::json!({"type": "assistant", "message": {}}).to_string();
        assert_eq!(check_usage_fields(&[bad.clone()]).level, Level::Bad);

        // 只有 message.id 缺（会重复计 token）⇒ 报"要注意"，不是失效
        let no_id = serde_json::json!({
            "type": "assistant", "uuid": "u", "timestamp": day,
            "message": {"usage": {"input_tokens": 1}}
        })
        .to_string();
        assert_eq!(check_usage_fields(&[no_id]).level, Level::Warn);
    }

    #[test]
    fn render_puts_failures_first_and_counts_them() {
        let fs = vec![
            Finding::ok("没事的", "d"),
            Finding::bad("坏了的", "d"),
            Finding::warn("看看这个", "d"),
        ];
        let out = render(&fs);
        assert!(out.contains("1 项失效、1 项要注意、1 项正常"), "{out}");
        let (bad_at, warn_at, ok_at) = (
            out.find("坏了的").unwrap(),
            out.find("看看这个").unwrap(),
            out.find("没事的").unwrap(),
        );
        assert!(bad_at < warn_at && warn_at < ok_at, "失效项必须排最前：\n{out}");
    }

    #[test]
    fn hooks_check_flags_a_missing_exe_and_a_complete_registration() {
        let dir = crate::testtmp::TempDir::new("doctor", "hooks");
        // ① 注册完整、且 exe 存在
        let exe = dir.join("real.exe");
        std::fs::write(&exe, b"x").unwrap();
        let mut hooks = serde_json::Map::new();
        for ev in crate::hooks_install::EVENTS {
            hooks.insert(
                ev.to_string(),
                serde_json::json!([{"hooks":[{"type":"command","command":exe.display().to_string(),"args":["--hook"]}]}]),
            );
        }
        let good = dir.join("settings.json");
        std::fs::write(&good, serde_json::json!({"hooks": hooks}).to_string()).unwrap();
        assert_eq!(check_hooks(&good).level, Level::Ok);

        // ② 注册在，但 exe 被删了（`cargo clean` / 换目录）⇒ 必须报失效
        std::fs::remove_file(&exe).unwrap();
        let f = check_hooks(&good);
        assert_eq!(f.level, Level::Bad, "{f:?}");
        assert!(f.title.contains("不存在"), "{f:?}");

        // ③ 一个事件都没注册
        let empty = dir.join("empty.json");
        std::fs::write(&empty, "{}").unwrap();
        assert_eq!(check_hooks(&empty).level, Level::Bad);

        // ④ 文件根本不存在
        assert_eq!(check_hooks(&dir.join("nope.json")).level, Level::Bad);
    }
}
