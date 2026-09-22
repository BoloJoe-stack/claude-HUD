use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub fn read_new(path: &Path, offset: u64) -> std::io::Result<Option<(String, u64)>> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };

    let len = f.metadata()?.len();
    // 文件比记录的 offset 短，说明被截断或轮换了：从头再来
    let start = if len < offset { 0 } else { offset };

    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;

    // 只消费到最后一个换行为止。半行不记账，留到下一轮补齐后再处理，
    // 否则 JSON 会被从中间截断，那条记录永久丢失。
    let usable = buf
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);

    // 按字节记账。from_utf8_lossy 兜底，避免异常字节让挂件崩掉。
    let text = String::from_utf8_lossy(&buf[..usable]).to_string();
    Ok(Some((text, start + usable as u64)))
}

#[cfg(test)]
mod read_tests {
    use super::*;
    use crate::testtmp::TempDir;
    use std::fs;
    use std::io::Write;

    fn tempdir(tag: &str) -> TempDir {
        TempDir::new("tail", tag)
    }

    #[test]
    fn missing_file_returns_none() {
        let d = tempdir("missing");
        assert!(read_new(&d.join("nope.jsonl"), 0).unwrap().is_none());
    }

    #[test]
    fn reads_from_zero_when_offset_is_zero() {
        let d = tempdir("fromzero");
        let p = d.join("t.jsonl");
        fs::write(&p, "AAA\n").unwrap();
        let (text, off) = read_new(&p, 0).unwrap().unwrap();
        assert_eq!(text, "AAA\n");
        assert_eq!(off, 4);
    }

    #[test]
    fn second_call_returns_only_new_bytes() {
        let d = tempdir("incremental");
        let p = d.join("t.jsonl");
        fs::write(&p, "AAA\n").unwrap();
        let (_, off) = read_new(&p, 0).unwrap().unwrap();

        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"BBB\n").unwrap();
        drop(f);

        let (text, off2) = read_new(&p, off).unwrap().unwrap();
        assert_eq!(text, "BBB\n", "只能拿到新增字节，不能重读整个文件");
        assert_eq!(off2, 8);
    }

    #[test]
    fn no_new_bytes_yields_empty_text_and_same_offset() {
        let d = tempdir("nonew");
        let p = d.join("t.jsonl");
        fs::write(&p, "AAA\n").unwrap();
        let (_, off) = read_new(&p, 0).unwrap().unwrap();
        let (text, off2) = read_new(&p, off).unwrap().unwrap();
        assert_eq!(text, "");
        assert_eq!(off2, off);
    }

    #[test]
    fn truncated_file_is_reread_from_start() {
        // 文件被替换/轮换（比 offset 短）时必须从头读，否则会永久丢失后半段
        let d = tempdir("truncated");
        let p = d.join("t.jsonl");
        fs::write(&p, "AAAAAAA\n").unwrap();
        let (_, off) = read_new(&p, 0).unwrap().unwrap();
        assert_eq!(off, 8);

        fs::write(&p, "X\n").unwrap();
        let (text, off2) = read_new(&p, off).unwrap().unwrap();
        assert_eq!(text, "X\n");
        assert_eq!(off2, 2);
    }

    #[test]
    fn handles_utf8_multibyte_boundary() {
        // 必须按字节记账；若从多字节字符中间切开会得到非法 UTF-8
        let d = tempdir("utf8");
        let p = d.join("t.jsonl");
        fs::write(&p, "中文\n").unwrap();
        let (text, off) = read_new(&p, 0).unwrap().unwrap();
        assert_eq!(text, "中文\n");
        assert_eq!(off, 7); // 3 + 3 + 1 字节
    }

    #[test]
    fn partial_line_is_not_consumed() {
        // transcript 是边写边追加的，读到的最后一行可能只写了一半。
        // 若把半行也记账，剩下半行到达时前半行已被消费 —— JSON 从中间截断，
        // 这条记录彻底丢失去。必须只消费到最后一个换行为止。
        let d = tempdir("partial");
        let p = d.join("t.jsonl");
        fs::write(&p, "AAAA").unwrap(); // 无换行 = 半行
        let (text, off) = read_new(&p, 0).unwrap().unwrap();
        assert_eq!(text, "", "半行不该被读出");
        assert_eq!(off, 0, "没有完整行时 offset 必须停在原地");

        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"BBBB\n").unwrap();
        drop(f);

        let (text, off2) = read_new(&p, 0).unwrap().unwrap();
        assert_eq!(text, "AAAABBBB\n", "补齐后必须能拿到完整一行");
        assert_eq!(off2, 9);
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;
    use serde_json::json;

    fn assistant_usage(input: u64, cache_read: u64, cache_create: u64) -> String {
        json!({
            "type": "assistant",
            "message": { "usage": {
                "input_tokens": input,
                "cache_read_input_tokens": cache_read,
                "cache_creation_input_tokens": cache_create
            }}
        })
        .to_string()
    }

    fn tool_use(name: &str, input: serde_json::Value) -> String {
        json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "tool_use", "name": name, "input": input }
            ]}
        })
        .to_string()
    }

    #[test]
    fn context_tokens_sums_three_fields() {
        let text = format!("{}\n", assistant_usage(146, 362_880, 0));
        let d = parse_delta(&text, &Delta::default());
        // spec §4.2 实测样例
        assert_eq!(d.context_tokens, Some(363_026));
    }

    #[test]
    fn later_usage_overwrites_earlier() {
        let text = format!(
            "{}\n{}\n",
            assistant_usage(100, 0, 0),
            assistant_usage(200, 0, 0)
        );
        assert_eq!(parse_delta(&text, &Delta::default()).context_tokens, Some(200));
    }

    #[test]
    fn custom_title_wins_over_ai_title_and_is_last_wins() {
        // `custom-title` 行 = 用户用 `/rename`（或 VS Code 里给会话改名）起的名字。
        // 它是 VS Code 标题链的**第 1 优先级**，而在此之前我们一档都没读。
        let t = concat!(
            r#"{"type":"custom-title","customTitle":"第一次"}"#,
            "\n",
            r#"{"type":"custom-title","customTitle":"第二次"}"#,
            "\n",
        );
        let d = parse_delta(t, &Delta::default());
        assert_eq!(d.custom_title.as_deref(), Some("第二次"), "同名行 last-wins");
    }


    #[test]
    fn first_prompt_takes_the_first_plain_string_user_message() {
        // 判据与打断检测**共用同一条**：真实发言的 `content` 是**纯字符串**；
        // 工具回执 / 打断标记 / 系统注入的都是块数组，必须跳过。
        let t = concat!(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"a"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"第一句话"}}"#,
            "\n",
            r#"{"type":"user","message":{"content":"第二句话"}}"#,
            "\n",
        );
        let d = parse_delta(t, &Delta::default());
        assert_eq!(
            d.first_prompt.as_deref(),
            Some("第一句话"),
            "first-wins：取第一条真实发言，且跳过块数组"
        );
    }

    #[test]
    fn a_long_multiline_first_prompt_is_collapsed_and_capped() {
        // 会话名要画在**一行**里，而用户的第一句话可以是一整段（粘一篇文章进来）。
        let multiline = "第一行\n\n第二行   第三行";
        let t = format!(
            r#"{{"type":"user","message":{{"content":{}}}}}"#,
            serde_json::to_string(multiline).unwrap()
        );
        let d = parse_delta(&t, &Delta::default());
        assert_eq!(
            d.first_prompt.as_deref(),
            Some("第一行 第二行 第三行"),
            "换行与连续空格都压成单个空格"
        );

        let huge = "字".repeat(500);
        let t = format!(r#"{{"type":"user","message":{{"content":"{huge}"}}}}"#);
        let fp = parse_delta(&t, &Delta::default()).first_prompt.unwrap();
        assert!(
            fp.chars().count() <= 301 && fp.ends_with('…'),
            "要截到上限并以省略号收尾，实得 {} 字",
            fp.chars().count()
        );
    }

    #[test]
    fn a_background_agent_keeps_running_after_its_launch_ack() {
        // 用户 2026-09-15 报的："主 agent 停止工作了，但是子代理还在工作，HUD 里应该显示
        // 工作中才对"。根因就在这条：后台代理的 `tool_result` 是**启动回执**，而按
        // "收到 tool_result 就算结束"处理，它会在**启动那一刻**就被移除 ——
        // 于是"主代理已 Stop、后台代理还在干"时界面显示"完成/待命"，与事实相反。
        //
        // 下面两段 JSON **逐字取自真机转录**（只截短了尾部不影响判据的部分）。
        let launched = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"call_x","name":"Agent","input":{"description":"d","prompt":"p"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"tool_use_id":"call_x","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully. (This tool result is internal metadata — never quote or paste any part of it, including the agentId below, into a user-facing reply.)\nagentId: a2a47eb2a5a208469 (internal ID - do not mention to user.)\nThe agent is working in the background. You will be notified when it finishes."}]}]}}"#,
            "\n",
        );
        let d = parse_delta(launched, &Delta::default());
        assert!(
            d.pending_agents.is_empty(),
            "启动回执不该留在「前台在飞」的集合里"
        );
        assert_eq!(d.bg_agents.len(), 1, "后台代理必须仍然算「在跑」—— 本条钉的就是这个");

        // 它干完了：主转录里来一条 `<task-notification>`。
        //
        // ⚠️ **下面这条 fixture 逐字取自真机转录，形态必须保持原样**（`type` 是
        // `queue-operation`、`content` 在**顶层**且是**字符串**）。
        // 这条 fixture 原先写的是 `{"type":"user","message":{"content":"<task-notification>…"}}`
        // —— 那个形态**在本机 36 份转录里一条都不存在**（真的 366 条全在 `queue-operation` 上），
        // 于是测试绿着、生产里 `bg_agents` 只增不减：用户 2026-09-16 报的
        // "任务已经结束了还显示工作中"就是这个。
        // **夹具的形态如果和真实语料不一样，测的就是一个不存在的世界。**
        let finished = concat!(
            r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-09-16T01:04:00.000Z","sessionId":"s","content":"<task-notification>\n<task-id>a2a47eb2a5a208469</task-id>\n<tool-use-id>call_x</tool-use-id>\n<output-file>C:\\x</output-file>\n<status>completed</status>\n<summary>Agent \"d\" finished</summary>\n</task-notification>"}"#,
            "\n",
        );
        let d = parse_delta(finished, &d);
        assert!(d.bg_agents.is_empty(), "收到终止通知之后必须移除");
    }

    /// **反向对照**：一句普通的工具输出里**引用**了那句启动回执（`grep` 源码、打印转录片段
    /// 都会这样），**不许**因此凭空造出一个后台代理。
    ///
    /// 这不是假想：2026-09-16 查上面那条 bug 时，我自己的 `Bash` 探针把转录片段打了出来，
    /// 于是本会话凭空多出 **4 个**幽灵代理（它们的 `tool_use_id` 根本不对应任何 `Agent` 调用，
    /// 也就永远收不到终止通知）—— 这正是本仓已记过的"**测量会污染自己的输入**"那一脚，
    /// 只不过这次污染的是**产品状态**，不只是我的分析结论。
    #[test]
    fn a_bash_output_quoting_the_launch_ack_does_not_create_a_phantom_agent() {
        let t = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"call_bash1","name":"Bash","input":{"command":"grep -n 'Async agent launched successfully' src/transcript.rs"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"tool_use_id":"call_bash1","type":"tool_result","content":[{"type":"text","text":"source line 248 matches: Async agent launched successfully ... The agent is working in the background ..."}]}]}}"#,
            "\n",
        );
        for line in t.lines().filter(|l| !l.trim().is_empty()) {
            serde_json::from_str::<Value>(line).expect("夹具必须是合法 JSON");
        }
        let d = parse_delta(t, &Delta::default());
        assert!(
            d.bg_agents.is_empty() && d.pending_agents.is_empty(),
            "一句**引用了**回执措辞的普通工具输出不是子代理：bg={:?} pending={:?}",
            d.bg_agents,
            d.pending_agents
        );
    }

    /// **反向对照**：我自己在会话里读到/打印出那串 `<task-notification>` XML 时，
    /// **不许**把在跑的子代理清掉。
    ///
    /// 这不是假想：本仓自己的源码与测试夹具里就有这个字符串，我在会话里 `Read` 一次
    /// `transcript.rs`、或 `grep` 一次，它就会作为**我自己的工具输出**落进转录
    /// （真机实测本会话里就有这么两行）。所以判据必须是**结构**（顶层字符串 `content`），
    /// 不能是"这一行里出现这串字"。
    #[test]
    fn a_notification_inside_my_own_tool_output_does_not_clear_agents() {
        let launched = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"call_z","name":"Agent","input":{}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"tool_use_id":"call_z","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully. (This tool result is internal metadata — never quote or paste any part of it, including the agentId below, into a user-facing reply.)\nagentId: aaaaaaaa (internal ID - do not mention to user.)\nThe agent is working in the background. You will be notified when it finishes."}]}]}}"#,
            "\n",
        );
        let d = parse_delta(launched, &Delta::default());
        assert_eq!(d.bg_agents.len(), 1, "前置条件：后台代理在跑");

        // 我读了源码 / 打印了测试夹具 —— 那串 XML 出现在**工具输出**里（`message.content[]`）。
        let my_output = concat!(
            r#"{"type":"user","message":{"content":[{"tool_use_id":"call_read","type":"tool_result","content":[{"type":"text","text":"<task-notification>\n<tool-use-id>call_z</tool-use-id>\n<status>completed</status>\n</task-notification>"}]}]}}"#,
            "\n",
        );
        // 夹具自身必须是**合法 JSON**：不合法的话 `parse_delta` 会静默跳过这一行
        // （"坏行跳过而不是整体失败"），于是本用例会**空过**、变成一条永远为真的断言。
        for fixture in [launched, &my_output] {
            for line in fixture.lines().filter(|l| !l.trim().is_empty()) {
                serde_json::from_str::<Value>(line)
                    .expect("本例的夹具每一行都必须是合法 JSON —— 否则 parse_delta 会静默跳过它，用例空过");
            }
        }
        let d = parse_delta(my_output, &d);
        assert_eq!(
            d.bg_agents.len(),
            1,
            "我自己的工具输出里出现这串 XML **不是**完成信号（否则读一次源码就会把在跑的子代理全清掉）：{:?}",
            d.bg_agents
        );
    }


    /// `last_line_at` = 这份转录里**最后一条对话行**（`assistant` / `user`）的时间戳
    /// （跨轮保留、取 max）。⚠️ **杂项行（`system` / `attachment` / `file-history-snapshot` /
    /// `last-prompt` / `cost-state` / `queue-operation`）不算** —— 那是 Claude Code 在做家务，
    /// 把它们算成"转录动过"会让一个已经完成的会话永远显示"工作中"（见 `parse_delta` 里的现场）。
    ///
    /// 它是"活动胜过终态"那条覆盖的唯一输入，所以两件事都要钉：**能解析**（含带毫秒与
    /// 带时区偏移两种写法）与**取的是 max 而不是最后一行**。
    #[test]
    fn the_delta_remembers_when_the_transcript_last_moved() {
        // 时间戳两种写法都要认：转录里实测是 `…Z`，但偏移量写错会**静默错 8 小时**。
        assert_eq!(parse_ts("2026-09-16T02:11:36.132Z"), Some(1789524696));
        assert_eq!(
            parse_ts("2026-09-16T10:11:36+08:00"),
            Some(1789524696),
            "同一时刻的 +08:00 写法必须得到同一个 epoch"
        );
        assert_eq!(parse_ts("2026-09-16T02:11:36Z"), Some(1789524696), "没有毫秒也要认");
        assert_eq!(parse_ts("不是时间"), None, "解析不了就 None，不 panic");
        assert_eq!(parse_ts(""), None);

        let d = parse_delta(
            concat!(
                r#"{"type":"assistant","timestamp":"2026-09-16T02:11:36.000Z","message":{"content":[]}}"#,
                "\n",
                // 后写的一行时间**更早**（同一毫秒内的乱序 / 时钟回拨）：max 语义必须留住大的那个
                r#"{"type":"assistant","timestamp":"2026-09-16T02:11:30.000Z","message":{"content":[]}}"#,
                "\n",
                // 没有时间戳的行不该把它清掉
                r#"{"type":"ai-title","aiTitle":"x"}"#,
                "\n",
            ),
            &Delta::default(),
        );
        assert_eq!(d.last_line_at, Some(1789524696), "取 max，不是取最后一行");

        // 跨轮保留：再折一段更旧的进去，不能把它拉回去
        let d2 = parse_delta(
            r#"{"type":"assistant","timestamp":"2026-09-16T02:00:00.000Z","message":{"content":[]}}"#,
            &d,
        );
        assert_eq!(d2.last_line_at, Some(1789524696), "新的旧行不能把已记录的活动时间拉回去");
    }

    /// ⭐ **只有对话行能推进 `last_line_at`** —— 杂项行不算"在干活"。
    ///
    /// 真机现场（2026-09-18，会话 `02e4a8ec`）：`Stop` 之后 **186 秒**落了三行 `system`，
    /// 而 `view` 那条"转录动过 ⇒ 工作中"的覆盖据此把一个**已经完成**的会话永远显示成
    /// "工作中"（用户当天报的"明明完成了却还是显示工作中"）。夹具就是那个形态 ——
    /// 时间戳逐字取自那三条行。
    ///
    /// **反向对照**：真来一条对话行，活动时间必须照旧推进 —— 否则"活动胜过终态"那条
    /// 修复（09-16 报的"在工作却显示完成"）会整个失效。
    #[test]
    fn housekeeping_lines_do_not_count_as_activity() {
        let assistant_at =
            r#"{"type":"assistant","timestamp":"2026-09-18T08:21:50.000Z","message":{"content":[]}}"#;
        let system_a = r#"{"type":"system","timestamp":"2026-09-18T08:44:30.084Z","uuid":"29639bf1"}"#;
        let system_b = r#"{"type":"system","timestamp":"2026-09-18T08:47:36.534Z","uuid":"c476816b"}"#;
        let d = parse_delta(
            &format!("{assistant_at}\n{system_a}\n{system_b}\n"),
            &Delta::default(),
        );
        assert_eq!(
            d.last_line_at,
            Some(parse_ts("2026-09-18T08:21:50.000Z").unwrap()),
            "system 行是做家务，不该推进「这份转录最后动过」的时间"
        );

        // 反向对照：一条 `user`（人打的字 / 工具结果）⇒ 必须推进
        let user_at =
            r#"{"type":"user","timestamp":"2026-09-18T09:00:00.000Z","message":{"content":[]}}"#;
        let d2 = parse_delta(&format!("{user_at}\n"), &d);
        assert_eq!(
            d2.last_line_at,
            Some(parse_ts("2026-09-18T09:00:00.000Z").unwrap()),
            "对话行必须照旧推进活动时间"
        );
    }

    #[test]
    fn a_foreground_agent_is_removed_by_its_own_result() {
        // **反向对照**：前台代理的 `tool_result` 就是它的结果，必须移除。
        // 别把"后台要等通知"那条规则用到所有代理上 —— 那样前台代理永远收不了工，
        // 界面会把每个已完成的子代理都算成"还在跑"。
        let t = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"call_y","name":"Agent","input":{}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"content":[{"tool_use_id":"call_y","type":"tool_result","content":[{"type":"text","text":"子代理的真实结果：一段普通总结。"}]}]}}"#,
            "\n",
        );
        let d = parse_delta(t, &Delta::default());
        assert!(d.pending_agents.is_empty(), "前台代理收到结果就该收工");
        assert!(d.bg_agents.is_empty(), "不能被误判成后台代理");
    }

    #[test]
    fn ai_title_line_sets_name() {
        let text = r#"{"type":"ai-title","aiTitle":"示例项目报告排版修复","sessionId":"s"}"#;
        let d = parse_delta(&format!("{}\n", text), &Delta::default());
        assert_eq!(d.ai_title.as_deref(), Some("示例项目报告排版修复"));
    }

    #[test]
    fn bash_tool_detail_is_the_command() {
        let text = format!("{}\n", tool_use("Bash", json!({"command": "cargo test"})));
        let d = parse_delta(&text, &Delta::default());
        assert_eq!(d.last_tool.as_deref(), Some("Bash"));
        assert_eq!(d.last_tool_detail.as_deref(), Some("cargo test"));
        assert_eq!(d.step_count, 1);
    }

    #[test]
    fn file_tool_detail_is_the_basename() {
        let text = format!(
            "{}\n",
            tool_use("Edit", json!({"file_path": "D:\\projects\\app\\src\\app.rs"}))
        );
        let d = parse_delta(&text, &Delta::default());
        assert_eq!(d.last_tool.as_deref(), Some("Edit"));
        assert_eq!(d.last_tool_detail.as_deref(), Some("app.rs"));
    }

    #[test]
    fn real_user_message_resets_step_count() {
        let steps = format!(
            "{}\n{}\n",
            tool_use("Read", json!({"file_path": "a.rs"})),
            tool_use("Bash", json!({"command": "ls"}))
        );
        let before = parse_delta(&steps, &Delta::default());
        assert_eq!(before.step_count, 2);

        // 真实用户发言（content 是纯字符串）→ 归零
        let user = r#"{"type":"user","message":{"content":"继续修排版"}}"#;
        let after = parse_delta(&format!("{}\n", user), &before);
        assert_eq!(after.step_count, 0);
    }

    #[test]
    fn tool_result_carrier_does_not_reset_step_count() {
        // tool_result 也是 type=="user"，但它不是"新一轮"，不能归零
        let steps = tool_use("Bash", json!({"command": "ls"}));
        let before = parse_delta(&format!("{}\n", steps), &Delta::default());
        let result = json!({
            "type": "user",
            "message": { "content": [
                { "type": "tool_result", "content": "ok" }
            ] }
        })
        .to_string();
        let after = parse_delta(&format!("{}\n", result), &before);
        assert_eq!(after.step_count, 1, "工具结果不能把步数清零");
    }

    #[test]
    fn interrupted_marker_is_detected() {
        let line = r#"{"type":"user","interruptedMessageId":"msg-1","message":{"content":""}}"#;
        let d = parse_delta(&format!("{}\n", line), &Delta::default());
        assert!(d.interrupted);
    }

    /// 打断标记属于**当前回合**：一次打断标志着回合的结束，其后用户再次发言就是新回合。
    /// 不复位的话标志会永久粘住 —— 挂件会把一个早被打断、现在正常干活的会话永远显示成
    /// "被打断"（`transcript_offset` 按 Ruling #6 不持久化，重启后全量重扫更会把历史里的
    /// 打断标记立刻重新置上）。
    #[test]
    fn real_user_message_clears_the_interrupt_flag() {
        let marker = r#"{"type":"user","interruptedMessageId":"msg-1","message":{"content":""}}"#;
        let after_marker = parse_delta(&format!("{}\n", marker), &Delta::default());
        assert!(after_marker.interrupted, "打断行必须先置位");

        // 真实用户发言（content 是纯字符串）→ 新回合开始，旧打断标记失效
        let user = r#"{"type":"user","message":{"content":"继续修排版"}}"#;
        let after_user = parse_delta(&format!("{}\n", user), &after_marker);
        assert!(!after_user.interrupted, "新回合开始后不得继续显示被打断");
        assert_eq!(after_user.step_count, 0, "复位与归零是同一个判据");
    }

    /// 负向：复位判据**不是**"任何 user 行"。tool_result 载体也是 type=="user"，
    /// 但它是工具回执、不是新一轮，不得清掉打断标记。
    #[test]
    fn tool_result_carrier_does_not_clear_the_interrupt_flag() {
        let marker = r#"{"type":"user","interruptedMessageId":"msg-1","message":{"content":""}}"#;
        let after_marker = parse_delta(&format!("{}\n", marker), &Delta::default());
        assert!(after_marker.interrupted);

        let result = json!({
            "type": "user",
            "message": { "content": [
                { "type": "tool_result", "content": "ok" }
            ] }
        })
        .to_string();
        let after = parse_delta(&format!("{}\n", result), &after_marker);
        assert!(after.interrupted, "工具回执不是新一轮，不得清掉打断标记");
    }

    /// 同一 chunk 内的先后顺序必须被遵守（`refresh` 一次可能拿到含多行的块）。
    #[test]
    fn interrupt_flag_reset_respects_line_order_within_a_chunk() {
        let marker = r#"{"type":"user","interruptedMessageId":"msg-1","message":{"content":""}}"#;
        let user = r#"{"type":"user","message":{"content":"继续修排版"}}"#;

        // [用户发言, 打断] → 最后发生的是打断，保持置位
        let text = format!("{}\n{}\n", user, marker);
        assert!(parse_delta(&text, &Delta::default()).interrupted, "后置的打断必须生效");

        // [打断, 用户发言] → 新回合，复位
        let text = format!("{}\n{}\n", marker, user);
        assert!(!parse_delta(&text, &Delta::default()).interrupted, "后置的用户发言必须复位");
    }

    // ---- 子代理（`Agent` 工具调用的配对）--------------------------------------
    // 真机 transcript 的形态：`tool_use` 块带 `id`，收工时的回执是**主文件里**一条
    // `type=="user"` 行，其 content 数组里有 `{"type":"tool_result","tool_use_id":<id>}`。

    fn agent_use(id: &str) -> String {
        json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "tool_use", "id": id, "name": "Agent",
                  "input": { "subagent_type": "general-purpose", "description": "查字体" } }
            ]}
        })
        .to_string()
    }

    fn agent_result(id: &str) -> String {
        json!({
            "type": "user",
            "message": { "content": [
                { "type": "tool_result", "tool_use_id": id, "content": "done" }
            ]}
        })
        .to_string()
    }

    #[test]
    fn no_agent_tool_use_means_no_pending_subagents() {
        // ④ 的"无子代理时不误报"分支：普通 transcript（用户发言 + 普通工具）里
        // 一个 `Agent` 都没有 → 集合必须为空。空集合是"不显示"的唯一判据。
        let text = format!(
            "{}\n{}\n",
            r#"{"type":"user","message":{"content":"帮我看看排版"}}"#,
            tool_use("Bash", json!({"command": "cargo test"}))
        );
        let d = parse_delta(&text, &Delta::default());
        assert!(d.pending_agents.is_empty(), "没有 Agent 调用就不该有在跑的子代理");
        assert_eq!(d.step_count, 1, "普通工具照常计步");
    }

    #[test]
    fn agent_without_its_tool_result_counts_as_one_running_subagent() {
        let d = parse_delta(&format!("{}\n", agent_use("call_1")), &Delta::default());
        assert_eq!(d.pending_agents.len(), 1, "回了 tool_use 但没回 tool_result = 还在跑");
        assert!(d.pending_agents.contains("call_1"));
    }

    #[test]
    fn tool_result_clears_exactly_the_matching_subagent() {
        // 两个并行子代理，先回一个：另一个仍在跑（计数 1，不是 0 也不是 2）。
        let text = format!(
            "{}\n{}\n{}\n",
            agent_use("call_1"),
            agent_use("call_2"),
            agent_result("call_1")
        );
        let d = parse_delta(&text, &Delta::default());
        assert_eq!(d.pending_agents.len(), 1);
        assert!(d.pending_agents.contains("call_2"), "减掉的必须是收到回执的那一个");
        assert!(!d.pending_agents.contains("call_1"));

        // 再回一个 → 归零
        let d = parse_delta(&format!("{}\n", agent_result("call_2")), &d);
        assert!(d.pending_agents.is_empty(), "回执到齐后必须归零");
    }

    #[test]
    fn non_agent_tool_result_does_not_disturb_the_count() {
        // 负向：普通工具（Bash/Read）的回执不能被当成子代理收工。若判据写成
        // "见到任何 tool_result 就减一"，这里会从 1 掉到 0。
        let text = format!(
            "{}\n{}\n{}\n",
            agent_use("call_1"),
            tool_use("Bash", json!({"command": "ls"})),
            agent_result("other_call")
        );
        let d = parse_delta(&text, &Delta::default());
        assert_eq!(d.pending_agents.len(), 1, "无关回执不得清掉正在跑的子代理");
    }

    #[test]
    fn tool_result_carrier_still_counts_as_a_carrier_after_the_agent_scan() {
        // 回归护栏：逐块扫 tool_result 的循环插在 user 分支里，不能把
        // "回执行不是新一轮" 这条既有性质碰掉（否则每条回执都会把步数清零）。
        let steps = tool_use("Bash", json!({"command": "ls"}));
        let before = parse_delta(&format!("{}\n", steps), &Delta::default());
        assert_eq!(before.step_count, 1);
        let after = parse_delta(&format!("{}\n", agent_result("call_x")), &before);
        assert_eq!(after.step_count, 1, "回执行仍不是新一轮");
    }

    #[test]
    fn interrupt_marker_clears_pending_subagents() {
        // 有界兜底：被打断的回合不会再有前台子代理的回执，不清空就会永久显示
        // "子代理 1"。真机实测这条平时不触发（134 个打断标记旁 0 个未配对 Agent），
        // 所以它只负责"最坏情况下跟着回合一起结束"。
        let before = parse_delta(&format!("{}\n", agent_use("call_1")), &Delta::default());
        assert_eq!(before.pending_agents.len(), 1);

        let marker = r#"{"type":"user","interruptedMessageId":"msg-1","message":{"content":""}}"#;
        let after = parse_delta(&format!("{}\n", marker), &before);
        assert!(after.pending_agents.is_empty(), "被打断的回合不留挂在跑的子代理");
        assert!(after.interrupted, "清空子代理不得影响打断标记本身");
    }

    #[test]
    fn pending_subagents_survive_a_chunk_with_no_new_agent_lines() {
        // 集合与 `prior` 的其它字段一样必须跨轮次保留：一轮只拿到半条 usage 行时，
        // 正在跑的子代理不能被顺手清掉。
        let before = parse_delta(&format!("{}\n", agent_use("call_1")), &Delta::default());
        let after = parse_delta(&format!("{}\n", assistant_usage(1, 2, 3)), &before);
        assert_eq!(after.pending_agents, before.pending_agents);
    }

    #[test]
    fn malformed_lines_are_skipped_without_losing_the_rest() {
        let text = format!("{{ not json\n{}\n", tool_use("Bash", json!({"command": "ls"})));
        let d = parse_delta(&text, &Delta::default());
        assert_eq!(d.step_count, 1, "坏行跳过，后面的行仍要处理");
    }

    #[test]
    fn prior_is_preserved_when_delta_has_no_new_info() {
        let prior = Delta {
            ai_title: Some("旧标题".into()),
            context_tokens: Some(1000),
            ..Default::default()
        };
        let d = parse_delta("", &prior);
        assert_eq!(d, prior, "没有新信息时必须原样保留 prior");
    }
}

use serde_json::Value;
use std::collections::BTreeSet;

/// 子代理是通过 `Agent` 工具调用派发的，所以"哪个工具代表一个子代理"就是这一个名字。
const AGENT_TOOL: &str = "Agent";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Delta {
    pub context_tokens: Option<u64>,
    pub ai_title: Option<String>,
    pub last_tool: Option<String>,
    pub last_tool_detail: Option<String>,
    pub interrupted: bool,
    pub step_count: u32,
    /// **仍在跑的子代理**：`Agent` 工具调用的 `tool_use` id 集合。
    ///
    /// 数据源调研（2026-09-14，用户要求"显示子代理任务情况"）：
    ///
    /// - 选了**主 transcript 里 `Agent` 的 `tool_use` 与 `tool_result` 配对** ——
    ///   一个 `Agent` 调用尚未收到对应 `tool_result` 即表示该子代理仍在跑。实测
    ///   本机 139 份主 transcript 里 82 次 `Agent` 调用**全部**在同一个文件里配上
    ///   了回执（未配对数 = 0），所以这条判据在"没有子代理"时**不会误报**，
    ///   也不需要任何超时窗口。
    /// - **不用 `pendingBackgroundAgentCount`**：它只出现在 `system` /
    ///   `turn_duration` 行上，是"回合结束那一刻"的快照，回合不结束就不更新 ——
    ///   拿它当实时计数会永远停在旧值（实测某会话最后一次是 08:52 写的 1，08:53
    ///   仍有新的 assistant 行，之后没有任何行把它改回 0）。
    /// - **不用 transcript 的 `isSidechain`**：子代理的对话记录写在
    ///   `<transcript去后缀>/subagents/agent-*.jsonl` **另一个文件**里（实测
    ///   25509 行 sidechain=true 全在那些文件、57632 行 sidechain=false 全在主文件），
    ///   主文件里恒为 false。它对"这是不是子代理的行"有用，对"现在有几个在跑"
    ///   没用；靠目录里文件的新鲜度猜则要引入一个没有依据的超时阈值。
    /// - **不用 hook 的 `SubagentStart` / `SubagentStop`**：本项目当前不注册它们，
    ///   而 spec §6 的事件集是固定的 8 个；改注册会动用户的全局
    ///   `~/.claude/settings.json`，属必须由用户明确同意的外部副作用。
    ///
    /// 集合（而不是计数）：`tool_result` 只带得回 `tool_use_id`，要正确地"减一"
    /// 就必须知道是哪一个。并行派发因此天然成立。
    pub pending_agents: BTreeSet<String>,

    /// **后台**子代理：已收到"已在后台启动"回执、**还没等到终止通知**的 id。
    ///
    /// ⚠️ 为什么要与 `pending_agents` 分开：**后台代理的 `tool_result` 是启动回执，
    /// 不是完成**（内容是 `Async agent launched successfully… The agent is working in
    /// the background`）。按"收到 `tool_result` 就算结束"处理，它们一启动就被当成跑完 ——
    /// 于是"主代理已经 `Stop`、后台代理还在干"时界面显示"完成/待命"，**与事实相反**。
    /// 用户 2026-09-15 实测报的就是这个。
    ///
    /// 后台代理真正的完成信号是主转录里的一条 `<task-notification>`（带 `<tool-use-id>`）。
    /// 实测其 `<status>` 只有 `completed` / `failed` / `killed` 三种、**没有** `running`，
    /// 所以见到匹配的 `tool-use-id` 就可以移除，不必再筛状态。
    ///
    /// ⚠️ **它长在哪一行上，曾经搞错过一次**（2026-09-16 用户报"任务已结束仍显示工作中"）：
    /// 那条通知是 **`{"type":"queue-operation","operation":"enqueue","content":"<task-notification>…"}`**
    /// —— `content` 在**顶层**、是**字符串**；而本函数原来只在 `type == "user"` 的分支里找它，
    /// 于是**一条真通知都没读到过**，`bg_agents` 只增不减 ⇒ 主代理 `Stop` 之后界面永远"工作中"。
    ///
    /// 取证（全机 36 份转录）：带 `<task-notification>` 且 `content` 是顶层字符串的行共 **366** 条，
    /// **全部**是 `queue-operation`；挂在 `user` 行上的 **0** 条。也就是说原来的判据**在真数据上从未命中**，
    /// 而当时那条测试用的是一条 `user` 行 fixture —— **夹具的形态在真实语料里根本不存在，测试于是给了假绿**。
    /// 现在按**结构**认（见 `is_notification_carrier`），并配反向对照
    /// `a_notification_inside_my_own_tool_output_does_not_clear_agents`。
    pub bg_agents: BTreeSet<String>,

    /// 这份转录里**最后一条带时间戳的行**是什么时候写的（epoch 秒）。
    ///
    /// 存在的唯一理由：**hook 只在回合边界上说话，而"回合边界"不等于"人在不在干活"**。
    /// 有两段真实存在的工作期**一个 hook 事件都不会发**：
    ///
    /// 1. **权限批了之后**。`Notification(permission_prompt)` 把状态推到「等待确认」，
    ///    而用户点"允许"**不会触发任何已注册的事件** —— 从那一刻起到本回合结束，
    ///    状态机里没有任何东西能把「等待确认」解掉（`model::transition` 里 Waiting 的出路
    ///    只有下一次 `UserPromptSubmit` / `Stop` / `SessionEnd` 等回合级事件）。
    /// 2. **后台子代理干完、主代理被自动唤醒**。这种回合是 `<task-notification>` 驱动的，
    ///    **没有 `UserPromptSubmit`**，于是上一个 `Stop` 留下的「完成」会一直挂到本回合结束。
    ///
    /// 两段的症状一样：**明明在干活，界面显示"等待确认"或"完成"**（用户 2026-09-16 报的）。
    /// 转录是唯一能看见"此刻真的在动"的信号 —— 状态文件只在事件边界更新，转录是**流**。
    ///
    /// 用法见 `view::build_rows_with_deltas` 里的"活动胜过终态"那条覆盖。
    pub last_line_at: Option<i64>,

    // ---- 会话名（2026-09-15 补；照 VS Code 的优先级链对齐）----------------------
    //
    // VS Code 显示会话标题时的规则是从 `extension.js` 里逐字读出来的：
    //
    //     customTitle > aiTitle > lastPrompt > summaryHint > firstPrompt
    //
    // 我们原来只实现了 `aiTitle` 一档，取不到就退回 cwd 末段 —— 于是**同一个目录下
    // 的多个会话会重名**（都叫 `claude`），而 VS Code 在那种情况下显示的是你的话。
    // 下面三档把这条链补齐（`summaryHint` 是扩展自己派生的，转录里没有对应字段，跳过）。

    /// **用户给会话起的名字**（Claude Code 的 `/rename`，或 VS Code 里给会话改名）。
    ///
    /// 转录里的形态是 `{"type":"custom-title","customTitle":"…"}`，**last-wins**。
    /// 它在 VS Code 的链里是**第 1 优先级**，所以排在 `ai_title` 之前。
    pub custom_title: Option<String>,
    /// **第一条真实用户发言**（**first-wins**，与上面两个的 last-wins 相反）。
    ///
    /// 转录里没有现成字段，得自己取。判据与打断检测**共用同一条**：真实发言的
    /// `content` 是**纯字符串**，而工具回执 / 打断标记 / 系统注入的都是块数组。
    /// VS Code 标题链最后一档 `firstPrompt` 就是它。
    pub first_prompt: Option<String>,
}

/// 把一段多行文本压成**单行**，并截到上限。
///
/// 两件事都是必需的：会话名要画在**一行**里，换行会把版面撑破；而用户的第一句话可能
/// 是一整段（粘一篇文章进来），不设上限就白占内存、还会让每次渲染都去量一个巨大的串。
fn collapse_ws(s: &str) -> String {
    const MAX_CHARS: usize = 300;
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= MAX_CHARS {
        return one;
    }
    let mut cut: String = one.chars().take(MAX_CHARS).collect();
    cut.push('…');
    cut
}

/// 这条 `tool_result` 是不是**后台子代理的启动回执**。
///
/// ⚠️ **这是文本匹配，没有结构性标志可用** —— 实测这个块的字段与普通工具结果完全一样
/// （只有 `tool_use_id` / `type` / `content`），区分只能靠内容里的固定措辞。
/// 所以 Claude Code 一改说法它就会**静默失效**，表现是"后台代理一启动就被当成跑完"，
/// 也就是本 bug 复发。同时认**两句**（实测回执里两句都有），把单点失效的概率降一半；
/// 真失效了也不会崩，只是退回改造前的行为。
fn is_async_launch_ack(block: &Value) -> bool {
    const MARKERS: [&str; 2] = [
        "Async agent launched successfully",
        "The agent is working in the background",
    ];
    let s = block.get("content").map(|c| c.to_string()).unwrap_or_default();
    MARKERS.iter().any(|m| s.contains(m))
}

/// 转录里的 RFC3339 时间戳（`2026-09-16T02:11:36.132Z`）→ epoch 秒。
///
/// 解析不了就返回 `None`（**不 panic、不猜**）：转录可能被并发截断、也可能哪天换个格式，
/// 而这种"读不到时间"只该让"活动胜过终态"这条覆盖**失效**（退化成今天的行为），
/// 不该把整个解析拖垮。
///
/// 用 `chrono` 而不是自己切字符串：`Z` 与 `+08:00` 两种写法都要认，偏移量算错会**静默**错 8 小时。
pub fn parse_ts(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.timestamp())
}

/// 这一行是不是"系统注入的通知行"—— 判据是**结构**，不是行类型名。
///
/// 形态（真机实测）：`{"type":"queue-operation","operation":"enqueue","content":"<task-notification>…"}`
/// —— **顶层** `content`、**字符串**。会话里的正常发言与工具输出都不长这样：
/// 那些的文字在 `message.content` / `message.content[].text` 里，顶层 `content` 根本不是字符串。
///
/// 为什么把判据定在"顶层字符串"而不是"`type == "queue-operation"`"：
/// ① 类型名是 Claude Code 的实现细节，换个名字本判据就静默失效（这次踩的就是"判据依赖了
///    一个没核实的形态"）；② 顶层字符串是**系统注入**的结构特征。
///
/// 为什么**不能**改成"任何一行里含 `<task-notification>` 就算"：这个仓库自己的源码与测试夹具
/// 里就有这个字符串 —— 我在会话里 `Read` 一次 `transcript.rs`、或 `grep` 一次，那串 XML 就会
/// 作为**我自己的工具输出**进转录。按"出现即算"的话，读一次源码就会把在跑的子代理全清掉。
/// 反向对照测试钉的就是这一条。
fn is_notification_carrier(v: &Value) -> Option<&str> {
    let s = v.get("content").and_then(|c| c.as_str())?;
    s.contains("<task-notification>").then_some(s)
}

/// 从 `<task-notification>` 正文里取出 `<tool-use-id>`。
fn task_notification_tool_use_id(text: &str) -> Option<String> {
    const OPEN: &str = "<tool-use-id>";
    const CLOSE: &str = "</tool-use-id>";
    let s = text.find(OPEN)? + OPEN.len();
    let e = s + text[s..].find(CLOSE)?;
    Some(text[s..e].to_string())
}

/// 从工具名和入参里挑一个"一眼能认出在干什么"的短描述。
pub fn summarize_tool(name: &str, input: &Value) -> Option<String> {
    match name {
        "Bash" => input.get("command").and_then(|v| v.as_str()).map(|s| {
            let line = s.lines().next().unwrap_or("").trim();
            if line.chars().count() > 60 {
                line.chars().take(60).collect::<String>() + "…"
            } else {
                line.to_string()
            }
        }),
        "Read" | "Edit" | "Write" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|p| {
                p.replace('\\', "/")
                    .rsplit('/')
                    .next()
                    .unwrap_or(p)
                    .to_string()
            }),
        _ => None,
    }
}

fn content_has_tool_result(content: &Value) -> bool {
    content
        .as_array()
        .is_some_and(|arr| arr.iter().any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result")))
}

/// 折叠一段新增文本。`prior` 里已有的信息在没有新信息时原样保留。
pub fn parse_delta(text: &str, prior: &Delta) -> Delta {
    let mut d = prior.clone();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // 坏行跳过而不是整体失败——transcript 可能被并发截断
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        // "这份转录**真的动过**是什么时候"（跨行取 max，跨轮保留在 `Delta` 里）。
        //
        // ⚠️ **只认对话行（`assistant` / `user`）**，不认"任何带时间戳的行"（2026-09-18 修）。
        // 转录里还混着一批**杂项行**：`system` / `file-history-snapshot` / `last-prompt` /
        // `cost-state` / `attachment` / `queue-operation` —— 那是 Claude Code 在**做家务**，
        // 与"这个会话在不在干活"无关。原先一律计入，于是 `view` 那条"转录动过 ⇒ 工作中"
        // 的覆盖会被一次家务写入触发。真机现场（`02e4a8ec`）：`Stop` 之后 **186 秒**落了三行
        // `system`，一个**已经完成**的会话就被永远显示成"工作中"（用户当天报的就是这个）。
        //
        // 取 **max 而不是最后一行**：转录是追加写的，但**同一毫秒内的多行**顺序不保证，
        // 而我们要的是"最新活动"，多算一点无害、少算一点会让"活动胜过终态"漏判。
        let is_conversation = matches!(
            v.get("type").and_then(|t| t.as_str()),
            Some("assistant") | Some("user")
        );
        if is_conversation
            && let Some(ts) = v.get("timestamp").and_then(|t| t.as_str())
            && let Some(epoch) = parse_ts(ts)
        {
            d.last_line_at = Some(d.last_line_at.map_or(epoch, |p| p.max(epoch)));
        }

        // 打断标记：hook 侧永远感知不到 Esc，只有 transcript 里的 `interruptedMessageId`
        // 能看见它（spec §7 坑 1）。下面 `user` 分支的复位判据需要知道本行是不是它。
        let is_interrupt_marker = v.get("interruptedMessageId").is_some();
        if is_interrupt_marker {
            d.interrupted = true;
            // 被打断的回合里，还在跑的**前台**子代理不会再有自己的回执落到
            // transcript 上；不清空就会把它永久显示成"子代理 1"。这是一条**有界
            // 兜底**：真机实测 134 个打断标记旁有 0 个未配对的 `Agent` 调用（Claude
            // Code 会在写打断标记之前把在飞工具的回执补上），所以它平时不触发。
            //
            // 后台子代理不受影响：它们的 tool_result 在**启动那一刻**就已写回，
            // 从来就不在这个集合里，清空对它们是空操作。所以这条只会把"卡住不降"
            // 变成"跟着回合一起结束"，不会造成漏报。
            d.pending_agents.clear();
        }

        // 后台子代理**干完了**：主转录里来一条 `<task-notification>`。
        //
        // ⚠️ **必须在 `match` 之外**（原来它写在 `type == "user"` 那一支里，而真通知根本不在
        // `user` 行上 —— 于是这条判据在真数据上**从未命中过**，`bg_agents` 只增不减）。
        // 教训与本仓另一处同类：**"某段代码在跑"要确认它在生产路径上真的被执行**，
        // 而不是"它写的那个条件看起来对"。
        if let Some(text) = is_notification_carrier(&v)
            && let Some(id) = task_notification_tool_use_id(text)
        {
            d.bg_agents.remove(&id);
            // 顺带也从"前台在飞"里去掉：实测前台代理不发这条通知，但万一发了，它同样是终止信号。
            // 对不在集合里的 id 是空操作。
            d.pending_agents.remove(&id);
        }

        match v.get("type").and_then(|t| t.as_str()) {
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                    d.ai_title = Some(t.to_string());
                }
            }
            // 用户给会话起的名字（`/rename` 或 VS Code 里改名）。last-wins。
            Some("custom-title") => {
                if let Some(t) = v.get("customTitle").and_then(|t| t.as_str()) {
                    d.custom_title = Some(t.to_string());
                }
            }
            Some("assistant") => {
                let Some(msg) = v.get("message") else { continue };

                if let Some(u) = msg.get("usage") {
                    let sum = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
                        .iter()
                        .filter_map(|k| u.get(k).and_then(|n| n.as_u64()))
                        .sum::<u64>();
                    d.context_tokens = Some(sum);
                }

                if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                    for b in blocks {
                        if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                            continue;
                        }
                        if let Some(name) = b.get("name").and_then(|n| n.as_str()) {
                            // 子代理在跑：这一发 `Agent` 还**没有**收到对应回执。
                            if name == AGENT_TOOL {
                                if let Some(id) = b.get("id").and_then(|i| i.as_str()) {
                                    d.pending_agents.insert(id.to_string());
                                }
                            }
                            d.last_tool = Some(name.to_string());
                            let input = b.get("input").cloned().unwrap_or(Value::Null);
                            d.last_tool_detail = summarize_tool(name, &input);
                        }
                        d.step_count += 1;
                    }
                }
            }
            Some("user") => {
                let msg = v.get("message");
                let content = msg.and_then(|m| m.get("content"));
                // 第一条**真实用户发言** → `first_prompt`（first-wins，所以只在还空着时写）。
                // 判据与打断检测共用：**真实发言的 `content` 是纯字符串**，工具回执 /
                // 打断标记 / 系统注入的都是块数组。
                if d.first_prompt.is_none()
                    && let Some(Value::String(text)) = content
                {
                    let t = collapse_ws(text);
                    if !t.is_empty() {
                        d.first_prompt = Some(t);
                    }
                }
                // 子代理收工：`Agent` 的回执就在这里（实测形态 = 普通 `tool_result`
                // 块，`is_error` 真假都会写）。**必须逐块扫，且在下面的
                // `is_tool_result_carrier` 分支之外** —— 回执行本身正是 carrier，
                // 放在 `if !is_tool_result_carrier` 里等于永远不执行。
                for b in content.and_then(|c| c.as_array()).into_iter().flatten() {
                    if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                        continue;
                    }
                    let Some(id) = b.get("tool_use_id").and_then(|i| i.as_str()) else {
                        continue;
                    };
                    if is_async_launch_ack(b) {
                        // 后台代理：这一条是**启动回执**，从"在飞"挪到"后台在跑"。
                        //
                        // ⚠️ **必须先确认它本来就在"在飞"集合里**（= 真有对应的 `Agent`
                        // `tool_use` 被解析过）。`is_async_launch_ack` 是**文本匹配**，而那句
                        // 开场白会出现在**任何**工具输出里 —— 只要谁 grep 一次源码、读一次
                        // transcript、或把一段转录打印出来，那串字就会作为他**自己的工具输出**
                        // 进转录。无条件插入的话，这些 id 就变成**永远清不掉的幽灵代理**：
                        // 谁也发不出与它们配对的 `tool_result` 通知，于是"子代理 N"与
                        // "工作中"永远挂在那儿。2026-09-16 实测：本会话因为调查这个 bug 而
                        // 打印过转录片段，凭空多出 **4 个**幽灵代理。
                        if d.pending_agents.remove(id) {
                            d.bg_agents.insert(id.to_string());
                        }
                    } else {
                        // 前台代理：这一条就是结果，收工。
                        d.pending_agents.remove(id);
                        d.bg_agents.remove(id);
                    }
                }
                // 带 tool_result 的 user 行是"工具回执"，不是新一轮
                let is_tool_result_carrier = content.is_some_and(content_has_tool_result);
                if !is_tool_result_carrier {
                    d.step_count = 0;

                    // 打断标记属于**当前回合**：一次打断标志着回合的结束，其后用户再次
                    // 发言就是新回合，旧标记已陈旧，必须复位 —— 不复位会让一个早被打断、
                    // 现在正常干活的会话被永久显示成"被打断"（挂件重启时会全量重扫
                    // transcript，历史里的标记还会被立刻重新置上）。复位与归零共用这一个
                    // 判据（`type=="user"` 且 content 不含 tool_result），不引入新概念。
                    //
                    // ⚠️ 但标记行**自身**也满足该判据（真机实测：`type=="user"`、
                    // content 是含 text 的块数组、不含 tool_result），故必须排除它 ——
                    // 否则它会在同一行上把刚置上的标志当场清掉，"被打断"永远看不见。
                    if !is_interrupt_marker {
                        d.interrupted = false;
                    }
                }
            }
            _ => {}
        }
    }

    d
}

