//! 计划 9b 的父进程链探针（Ruling B：原 Task 7 的 Step 5/6 整体移入 9b）。
//!
//! 9b 要回答的是：hook 进程被**谁** spawn —— 父进程若稳定指到 Claude Code 宿主，
//! `SessionState.claude_pid` 就有意义（可走真实存活检测）；若是某个中间 shell，
//! 就只能改用超时回收。
//!
//! # 结论（2026-09-15 取得）
//!
//! - hook 的 **hop 1 就是 `claude.exe`**，没有中间 shell（注册是 exec 形式、不过 shell）。
//! - **宿主 pid 对同一个 `session_id` 恒定**：一个会话 3/3、另一个 2/2。
//!
//! ⇒ **裁定：保留 `claude_pid`，走真实存活检测**（不搞超时回收）。实现分布在
//! `hook.rs`（`SessionStart` 抓一次）与 `ui.rs`（每轮判存活）。
//!
//! 取证过程还纠正过一次**我自己的误判**：先拿到的两份样本宿主 pid 不同，一度像是
//! "宿主会变"；补上 `session_id` 后才看出那两份本来就来自**不同会话**（本机同时开着 3 个）。
//! 这正是本项目的老坑 —— 手边一次观测不能当结论。
//!
//! 本模块保留下来，是因为取证还可能要复现，且这两个开关本身有诊断价值。
//! 进程表相关的底层已搬到 `procinfo.rs`（它现在也是生产代码，有真实消费者了）。
//!
//! # 两条启用方式
//!
//! 未启用时，正常 hook 路径只多一次环境变量查询加一次 `metadata`，其余一字不变：
//! 仍然静默、仍然 `exit 0`。

use crate::procinfo::{self, HOST_EXE};

/// 计划 9b 指定的环境变量名。
pub const DEBUG_ENV: &str = "CLAUDE_HUD_DEBUG_PPID";

/// **响亮**启用：设了环境变量 → 写文件**并**弹框。供显式调试用。
///
/// **判据是"设了且不是 0"**，不是严格等于 `"1"`：只认 `"1"` 会让 `=true` / `=yes`
/// 静默不生效，而"设了却没反应"正是本批要消灭的失效模式（9b 原样执行得到的就是
/// 一个"测不出来"的假结论）。
pub fn loud() -> bool {
    std::env::var_os(DEBUG_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// **安静**启用：存在哨兵文件即生效，**只写文件、不弹框**。
///
/// 为什么需要第二条路：环境变量那条有个硬伤 —— **hook 进程的环境变量无法由外部注入，
/// 必须在 Claude Code 启动时就带上**，于是"测一次"的代价是重启整个 Claude Code。
/// 这就是 9b 一直悬着没测的直接原因。哨兵文件是**运行时**读的：创建它，下一个 hook
/// 事件就生效，不用重启；测完删掉即可。
///
/// 它**不弹框**是刻意的：安静模式下每个事件都会跑一次探针，模态框会糊满用户屏幕，
/// 而且会挂在 Claude Code 的调用路径上等用户点"确定"。
pub fn quiet() -> bool {
    std::fs::metadata(crate::paths::real_probe_enable_path()).is_ok()
}

/// 探针是否启用（两条路任一）。
pub fn enabled() -> bool {
    loud() || quiet()
}

/// 探针输出文件的软上限：超过就先清空重来。
/// 安静模式下**每个** hook 事件都会追加一条样本，文件不能无限长。
const PROBE_FILE_MAX_BYTES: u64 = 256 * 1024;

/// 跑一次探针，把样本**追加**进 [`crate::paths::real_probe_path`]，
/// 返回给 MessageBox 显示的正文（已含文件路径）。
///
/// **未启用时返回 `None` 且不做任何事**（连文件都不碰）。
///
/// **为什么是追加而不是覆盖**：要回答的问题是"宿主 pid 对**同一个会话**稳不稳定" ——
/// 一条样本回答不了。样本里带上 `session_id` 与事件名，才能把不同会话的样本分开看。
/// 第一版是覆盖写，正是它给了上面那次误判可乘之机。
pub fn ppid_probe(session_id: &str, event: &str) -> Option<String> {
    if !enabled() {
        return None;
    }

    let me = unsafe { procinfo::GetCurrentProcessId() };
    let path = crate::paths::real_probe_path();

    // 先看要不要清空（清空后下面会重新写文件头）。
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > PROBE_FILE_MAX_BYTES {
        let _ = std::fs::remove_file(&path);
    }
    let fresh = std::fs::metadata(&path).map(|m| m.len() == 0).unwrap_or(true);

    let mut to_write = String::new();
    if fresh {
        to_write.push_str(
            "claude-hud ppid 探针输出（计划 9b 的取证）。每个 hook 事件追加一条样本。\n\
             要回答的问题：**同一个 session_id 的样本里，nearest_claude_pid 稳不稳定。**\n\
             稳定 → 保留 `claude_pid` 做存活检测；不稳定、或是中间 shell → 改用超时回收。\n\n",
        );
    }

    to_write.push_str(&format!(
        "===== {} | event={event} | session={session_id} =====\nhook_pid={me}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    match procinfo::snapshot() {
        Ok(table) => {
            to_write.push_str(&match procinfo::nearest_named(&table, me, HOST_EXE) {
                Some(p) => format!("nearest_claude_pid={p}\n"),
                None => format!("nearest_claude_pid=<链上无 {HOST_EXE}>\n"),
            });
            let chain = procinfo::ancestor_chain(&table, me);
            to_write.push_str(&format!("父进程链（由近到远，共 {} 层）：\n", chain.len()));
            if chain.is_empty() {
                to_write.push_str("  （空 —— 快照里查不到本进程，或它的 ppid 为 0）\n");
            }
            for (i, hop) in chain.iter().enumerate() {
                to_write.push_str(&format!("  {}  {}\n", i + 1, hop));
            }
        }
        Err(e) => to_write.push_str(&format!("进程表快照失败：{e}\n")),
    }
    to_write.push('\n');

    use std::io::Write as _;
    let res = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(to_write.as_bytes()));
    match res {
        Ok(()) => Some(format!("{to_write}（追加到 {})", path.display())),
        Err(e) => Some(format!("{to_write}写文件失败（{}）：{e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_is_off_when_neither_switch_is_on() {
        // 只钉"两个开关都没开 → 关"这一半：另一半要在**同一个进程**里改环境变量才测得了，
        // 而 `cargo test` 多线程共享环境，改了会影响别的用例（含并行测试）。
        // 哨兵文件同理 —— 它读的是机器上的真实路径，测试不该去创建/删除它。
        // 真机验证由探针本身完成（见报告）。
        if std::env::var_os(DEBUG_ENV).is_none() && !quiet() {
            assert!(!enabled(), "两个开关都没开时探针必须关闭");
            assert!(
                ppid_probe("s1", "Stop").is_none(),
                "未启用时探针不做任何事（连文件都不碰）"
            );
        }
    }
}
