use crate::state::SessionState;
use crate::transcript::{self, Delta};
use std::collections::HashMap;
use std::io;
use std::path::Path;

/// 刷新单个会话的 transcript 派生信息。
///
/// 只读新增字节（`transcript_offset` 之后的部分），因此对长会话也是常数开销。
///
/// `now` 由调用方传入而非内部取当前时间 —— 与 `build_rows` / `handle` 一致，
/// 也让时间相关的行为可被确定性测试（pre-flight Ruling #5）。
///
/// ⚠️ **本函数绝不写状态文件**（pre-flight Ruling #6）。每个状态文件只能有一个写者
/// （hook 侧）；挂件手里的内存副本可能是旧的（`state=working`），回写会覆盖 hook 刚
/// 写下的 `state=waiting` —— 原子写只保证不读到半截文件，不保证不丢更新。故
/// `transcript_offset` 与 `Delta` 一样只活在挂件进程内存里；挂件重启后重扫一次
/// transcript 作一次性开销。
pub fn refresh(
    s: &mut SessionState,
    deltas: &mut HashMap<String, Delta>,
    now: i64,
) -> io::Result<()> {
    // ---- 1. 先用**已经存在内存里的 delta** 定一次标题 ----------------------------
    //
    // ⚠️ 这一步必须在任何 `return` **之前**，而且必须用 delta 而不是本轮读到的文本。
    //
    // 为什么：状态文件**每轮都从磁盘重读**，而 `display_name` 在盘上**恒为 `None`**
    // （挂件不写回状态文件 —— Ruling #6，hook 也读不到 transcript）。所以每轮开始时
    // `s.display_name` 都是空的。若标题只在"本轮读到了新字节"那条路上赋值，那么
    // **空闲的轮次就会掉回 cwd 末段** —— 界面上就是标题每隔几秒在真标题和目录名之间
    // 来回闪。用户 2026-09-15 报的"过一会会弹出其他东西来"正是这个。
    //
    // delta 活在内存里、跨轮保留，所以由它定标题就与"本轮有没有新字节"无关。
    apply_title(s, deltas.get(&s.session_id));

    let Some(path) = s.transcript_path.clone() else {
        return Ok(());
    };

    let Some((text, new_offset)) = transcript::read_new(Path::new(&path), s.transcript_offset)?
    else {
        return Ok(());
    };

    if !text.is_empty() {
        let prior = deltas.get(&s.session_id).cloned().unwrap_or_default();
        let next = transcript::parse_delta(&text, &prior);

        // ---- 2. 新 delta 可能带来更高档的标题，再定一次 -------------------------
        apply_title(s, Some(&next));

        // spec §7 第一个坑：Esc 打断不触发任何 hook，只有这里能看见。
        // 只把"进行中/已完成"推进为 interrupted —— 不要把 waiting / error 覆盖掉。
        if next.interrupted && matches!(s.state.as_str(), "working" | "done" | "idle") {
            s.state = "interrupted".to_string();
            s.state_since = now;
        }

        deltas.insert(s.session_id.clone(), next);
    }

    s.transcript_offset = new_offset;
    Ok(())
}

/// 按 **VS Code 的优先级链**给 `s` 定名字（链从 `extension.js` 逐字读出，2026-09-15）：
///
/// ```text
/// customTitle  >  aiTitle  >  firstPrompt
/// ```
///
/// `summaryHint` 是扩展自己派生的、转录里没有对应字段，跳过。
///
/// ## 一处与 VS Code 的**有意偏离**（用户 2026-09-15 裁定"标题固定不动"）
///
/// VS Code 在 `aiTitle` 之后用 **`lastPrompt`**（最后一条 prompt）。它的会话列表是
/// **一次快照**，标题跟着对话跳动无所谓；而挂件**常驻桌面**，一个每隔几秒就变成
/// "你刚打的字"的标题比没有还糟。故跳过 `lastPrompt`，直接用 **`firstPrompt`** ——
/// 它是 first-wins，**天然稳定**，同样能回答"这个会话是干什么的"。
///
/// （补这条链是因为原先只有 `aiTitle` 一档、取不到就退回 cwd 末段，于是同一目录下的
/// 会话**全都同名**：实测三个会话全叫 `claude`。）
///
/// 空串一律当"没有"：`/rename ""` 之类的操作不该把标题清空成一片空白。
fn apply_title(s: &mut SessionState, d: Option<&Delta>) {
    let Some(d) = d else { return };
    let picked = [&d.custom_title, &d.ai_title, &d.first_prompt]
        .into_iter()
        .flatten()
        .map(|t| t.trim())
        .find(|t| !t.is_empty());
    if let Some(t) = picked {
        s.display_name = Some(t.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionState;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    use crate::testtmp::TempDir;

    fn tempdir(tag: &str) -> TempDir {
        TempDir::new("poller", tag)
    }

    fn session(transcript: PathBuf) -> SessionState {
        SessionState {
            session_id: "p1".into(),
            cwd: "D:\\w".into(),
            transcript_path: Some(transcript.to_string_lossy().to_string()),
            display_name: None,
            claude_pid: None,
            claude_start: None,
            nested: None,
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
    fn the_session_name_follows_the_vs_code_priority_chain() {
        // 链（从 `extension.js` 逐字读出，2026-09-15）：customTitle > aiTitle > firstPrompt。
        // **`lastPrompt` 有意跳过** —— 见 `apply_title` 的注释：它是"最后一条 prompt"，
        // 会让常驻挂件上的标题每隔几秒就变成你刚打的字。
        //
        // 这条链是**用户报的"无法正确得到会话标题"的修复**：原来只有 `aiTitle` 一档，
        // 取不到就退回 cwd 末段 —— 同目录下的会话因此全都同名。
        let d = tempdir("chain");
        let t = d.join("t.jsonl");
        // ⚠️ **必须以换行结尾**：`read_new` 故意只消费到最后一个换行为止（半行不记账，
        // 防止 JSON 从中间被截断后永久丢失）。没有结尾换行时**最后一行读不到** ——
        // 本用例第一版就栽在这儿：`custom-title` 正好是最后一行，于是永远不被解析，
        // 表现是"整条链都对、唯独最高那档不见了"。真实转录的最后一个字节就是 `\n`。
        let name_for = |lines: &str| -> Option<String> {
            fs::write(&t, format!("{lines}\n")).unwrap();
            let mut s = session(t.clone());
            let mut deltas = HashMap::new();
            refresh(&mut s, &mut deltas, 1000).unwrap();
            s.display_name
        };
        let first = r#"{"type":"user","message":{"content":"第一句话"}}"#;
        let ai = r#"{"type":"ai-title","aiTitle":"AI 标题"}"#;
        let custom = r#"{"type":"custom-title","customTitle":"我起的名字"}"#;

        assert_eq!(
            name_for(&[first, ai, custom].join("\n")).as_deref(),
            Some("我起的名字"),
            "customTitle 最高优先"
        );
        assert_eq!(
            name_for(&[first, ai].join("\n")).as_deref(),
            Some("AI 标题"),
            "没有 customTitle 就退到 aiTitle"
        );
        assert_eq!(
            name_for(first).as_deref(),
            Some("第一句话"),
            "最后退到第一条真实用户发言 —— **不再是 cwd 末段**（那正是三个会话同名的原因）"
        );
    }

    #[test]
    fn the_title_survives_a_poll_with_no_new_bytes() {
        // 用户 2026-09-15 报"过一会会弹出其他东西来"。根因就是这条：
        //
        // 状态文件**每轮都从磁盘重读**，而盘上的 `display_name` 恒为 `None`（挂件不
        // 写回 —— Ruling #6，hook 也读不到 transcript）。若标题只在"本轮读到了新字节"
        // 那条路上赋值，**空闲的轮次就会掉回 cwd 末段** —— 界面上标题在真标题和目录名
        // 之间来回闪。
        //
        // 这里手工复现"每轮都把 `s` 从盘上重建一遍"：第二轮的 `s.display_name` 先复位成
        // `None`，且 transcript 一个字节都没变。
        let d = tempdir("title-survives");
        let t = d.join("t.jsonl");
        fs::write(&t, format!("{}\n", r#"{"type":"ai-title","aiTitle":"真标题"}"#)).unwrap();

        let mut s = session(t);
        let mut deltas = HashMap::new();
        refresh(&mut s, &mut deltas, 1000).unwrap();
        assert_eq!(s.display_name.as_deref(), Some("真标题"));

        s.display_name = None; // ← 模拟"盘上的状态文件又被读了一遍"
        refresh(&mut s, &mut deltas, 2000).unwrap();
        assert_eq!(
            s.display_name.as_deref(),
            Some("真标题"),
            "没有新字节时标题也必须还在 —— 掉了就是界面上那个闪烁"
        );
    }

    #[test]
    fn the_fallback_title_does_not_change_as_the_conversation_goes_on() {
        // 用户裁定"标题固定不动"，所以兜底档用 `firstPrompt`（first-wins）而不是
        // `lastPrompt`（last-wins）。这条钉住那个选择：后面的话再多，标题也不许变。
        let d = tempdir("title-fixed");
        let t = d.join("t.jsonl");
        let first = r#"{"type":"user","message":{"content":"第一句话"}}"#;
        fs::write(&t, format!("{first}\n")).unwrap();

        let mut s = session(t.clone());
        let mut deltas = HashMap::new();
        refresh(&mut s, &mut deltas, 1000).unwrap();
        assert_eq!(s.display_name.as_deref(), Some("第一句话"));

        // 又聊了很多（含 `last-prompt` 行）—— 标题必须一动不动
        fs::write(
            &t,
            format!(
                "{first}\n{}\n{}\n",
                r#"{"type":"last-prompt","lastPrompt":"第十句话"}"#,
                r#"{"type":"user","message":{"content":"第十一句话"}}"#
            ),
        )
        .unwrap();
        s.display_name = None;
        refresh(&mut s, &mut deltas, 2000).unwrap();
        assert_eq!(s.display_name.as_deref(), Some("第一句话"), "标题必须固定不动");
    }

    #[test]
    fn refresh_advances_offset_and_parses_usage_and_title() {
        let d = tempdir("basic");
        let t = d.join("t.jsonl");
        fs::write(
            &t,
            concat!(
                r#"{"type":"ai-title","aiTitle":"示例项目报告排版修复"}"#, "\n",
                r#"{"type":"assistant","message":{"usage":{"input_tokens":146,"cache_read_input_tokens":362880,"cache_creation_input_tokens":0}}}"#, "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#, "\n"
            ),
        )
        .unwrap();

        let mut s = session(t);
        let mut deltas = HashMap::new();
        refresh(&mut s, &mut deltas, 1000).unwrap();

        assert!(s.transcript_offset > 0, "offset 必须推进");
        assert_eq!(s.display_name.as_deref(), Some("示例项目报告排版修复"));

        let d = deltas.get("p1").unwrap();
        assert_eq!(d.context_tokens, Some(363_026));
        assert_eq!(d.last_tool.as_deref(), Some("Bash"));
        assert_eq!(d.step_count, 1);
    }

    #[test]
    fn refresh_is_incremental_and_does_not_double_count() {
        let d = tempdir("incremental");
        let t = d.join("t.jsonl");
        // ⚠️ 结尾的 "\n" 是**必需**的：`read_new` 只消费到最后一个换行（Task 8 修的
        // "半行不记账"缺陷，由 `partial_line_is_not_consumed` 钉住）。fixture 少了它，
        // read_new 就返回空文本，本用例会退化成"什么都没测到"。
        fs::write(
            &t,
            concat!(
                r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"a.rs"}}]}}"#,
                "\n"
            ),
        )
        .unwrap();
        let mut s = session(t.clone());
        let mut deltas = HashMap::new();

        refresh(&mut s, &mut deltas, 1000).unwrap();
        assert_eq!(deltas["p1"].step_count, 1);

        // 没有新增内容时再刷一次，步数不能翻倍
        refresh(&mut s, &mut deltas, 1000).unwrap();
        assert_eq!(deltas["p1"].step_count, 1, "空刷新不得重复计数");

        // 追加一条后只增加 1
        let mut f = fs::OpenOptions::new().append(true).open(&t).unwrap();
        use std::io::Write;
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Bash","input":{{"command":"ls"}}}}]}}}}"#
        )
        .unwrap();
        drop(f);

        refresh(&mut s, &mut deltas, 1000).unwrap();
        assert_eq!(deltas["p1"].step_count, 2);
    }

    #[test]
    fn interrupted_marker_moves_state_to_interrupted() {
        let d = tempdir("interrupted");
        let t = d.join("t.jsonl");
        // 同上：结尾的 "\n" 必需，否则 read_new 视为"半行"而不消费。
        fs::write(
            &t,
            concat!(
                r#"{"type":"user","interruptedMessageId":"msg-1","message":{"content":""}}"#,
                "\n"
            ),
        )
        .unwrap();

        let mut s = session(t);
        let mut deltas = HashMap::new();
        refresh(&mut s, &mut deltas, 1000).unwrap();

        assert_eq!(s.state, "interrupted", "Esc 打断后状态必须变，不能卡在 working");
    }

    #[test]
    fn real_user_message_after_interrupt_marker_keeps_state_working() {
        // 账本 #99：**清除方向**没有任何测试。`[打断标记, 真实用户发言]` 是极常见的
        // 组合（按 Esc 之后马上重新提问）—— 用户发言意味着新回合已开始，状态必须停在
        // `working`。若这里被判成 `interrupted`，它还会**永久粘住**：`transcript_offset`
        // 按 Ruling #6 不持久化，挂件重启后全量重扫会把历史里的标记重新置上。
        let d = tempdir("interrupt-cleared");
        let t = d.join("t.jsonl");
        // 结尾的 "\n" 必需（read_new 只消费到最后一个换行），两行同属一个 chunk。
        fs::write(
            &t,
            concat!(
                r#"{"type":"user","interruptedMessageId":"msg-1","message":{"content":""}}"#,
                "\n",
                r#"{"type":"user","message":{"content":"继续修排版"}}"#,
                "\n"
            ),
        )
        .unwrap();

        let mut s = session(t);
        let mut deltas = HashMap::new();
        refresh(&mut s, &mut deltas, 1000).unwrap();

        assert_eq!(s.state, "working", "新回合已开始，不得显示为被打断");
        assert!(!deltas["p1"].interrupted, "delta 里的标记也必须复位");
    }

    #[test]
    fn missing_transcript_is_not_an_error() {
        let mut s = session(PathBuf::from("C:\\nope\\missing.jsonl"));
        let mut deltas = HashMap::new();
        assert!(refresh(&mut s, &mut deltas, 1000).is_ok());
        assert_eq!(s.transcript_offset, 0);
    }

    #[test]
    fn session_without_transcript_path_is_skipped() {
        let mut s = session(PathBuf::from("x"));
        s.transcript_path = None;
        let mut deltas = HashMap::new();
        assert!(refresh(&mut s, &mut deltas, 1000).is_ok());
    }
}
