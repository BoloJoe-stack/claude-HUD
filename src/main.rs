// 挂件是常驻桌面的小窗，绝不能带控制台黑框（spec §9）。
// 代价：本进程不再拥有控制台，`println!` / `eprintln!` 写出去没有任何人看得见 ——
// 所以 CLI 子命令（--install-hooks / --uninstall-hooks）的结果一律改走 show_message
// 的系统对话框；`--hook` 本就静默，不受影响。9c「错误必须原样呈现给用户」与
// spec §9「没有控制台黑框」由此同时成立。
#![windows_subsystem = "windows"]

// Ruling #7 的到期义务已于 Task 12 履行：Task 2–9 期间给每个新模块加的临时
// `#[allow(dead_code)]`（注释都写着"可达消费者在 Task 11 出现、届时删除"）已全部删除。
//
// **为什么到期日是 Task 12 而不是 Task 11：** T11 只接上了 ui 与 main，而
// `transcript::Delta` 的字段（context_tokens / ai_title / last_tool / last_tool_detail /
// step_count / interrupted）在 T12 的 Step 4–5 之前**没有任何读者**（T9 评审核查过：
// `grep parse_delta|Delta|summarize_tool` 在 transcript.rs 之外零匹配）。在 T11 删除只会
// 把降噪换成"field is never read"的真噪音。T12 接线完成后所有条目才全部可达。
//
// ⚠️ **原先这里接着写"`cargo build` 警告数为 0 正是删除的判据"—— 那句已作废。**
// `8f6bbfa` 把 Ruling #7 的到期判据改成了"剩余警告**逐条可解释**"。现在实测是 **7 条**
// 警告，逐条都有理由（测试专用辅助 / 保留的 schema 字段 / 计划明令保留的
// `Progress::Tasks` 与 `build_rows` 老签名 / M4 才用的 `aggregate`），清单在
// `docs/工作日志.md`。**不要照"警告数应为 0"去验收。**
mod paths;
mod config;
mod doctor;
mod hook;
mod model;
mod state;
mod hooks_install;
mod transcript;
mod usage;
mod view;

// ui::run 由本文件的参数分发直接调用，其内部条目因此都是**可达**的：
// 不适用 Ruling #7（那只覆盖"可达消费者要到后面才出现"的模块），故不加
// #[allow(dead_code)]。
mod ui;

// poller::refresh 由 ui 的轮询循环调用（每轮把 transcript 增量折叠进内存里的 Delta）。
// 同样不适用 Ruling #7：它不是"消费者要到后面才出现"的模块，故不加 #[allow(dead_code)]。
mod poller;

// probe::ppid_probe 由下面 `--hook` 分支调用（计划 9b 的父进程链探针，只在设了
// `CLAUDE_HUD_DEBUG_PPID` 时才真的做事）。故同样不加 #[allow(dead_code)]。
mod probe;

// 进程表查询（Win32）。9b 裁定"保留 `claude_pid` 做真实存活检测"之后它有了真实
// 消费者：hook 侧在 SessionStart 抓一次宿主 pid，挂件侧每轮判一次存活。见该文件顶部。
mod procinfo;

// 光标屏幕坐标（Win32）。缩放是"自己算"的，而算的时候**必须**有一个与窗口位置无关的
// 光标位置，否则窗口会被自己推着跑（2026-09-17 实测的卡死）。见该文件顶部。
mod cursor;

// 测试临时目录的统一辅助（唯一一个 `cfg(test)` 才编译的模块）。它存在的直接原因是
// 此前 7 个模块各写一份 `tempdir()`、且创建后都不回收 —— 跑一次测试就往 `%TEMP%` 里
// 丢几十个目录，跨任务累积到 3090 个（2026-09-15 实测并清理）。
#[cfg(test)]
mod testtmp;

// 系统对话框是本程序唯一的"给人看"通道（GUI 子系统下没有控制台可打印）。
// 抽成独立模块是为了让 `ui.rs` 也能用 —— 它有"中文字体缺失"要报。见 `dialog.rs` 顶部。
mod dialog;

use dialog::show_message;

use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // GUI 子系统下**任何** panic 都是无声死亡：默认 hook 往 stderr 写，而这个进程的
    // stderr 通向虚空（没有控制台）。用户看到的会是"双击了，什么都没发生"—— 正是
    // 9c / spec §4.3 要求消灭的那一种失效。所以换成走 MessageBox 的 hook，至少让
    // "崩了"这件事可见，并带上 panic 的原文与位置。
    //
    // **`--hook` 必须是例外**：那条路径被 Claude Code 调用，硬性要求静默、快速退出、
    // 绝不阻塞（失败也 exit 0）。一个模态框会直接挂在 Claude Code 的调用链路上等用户
    // 点"确定"。所以只在非 hook 模式装它。
    if args.first().map(String::as_str) != Some("--hook") {
        install_panic_hook();
    }

    match args.first().map(|s| s.as_str()) {
        Some("--hook") => {
            let mut raw = String::new();
            if std::io::stdin().read_to_string(&mut raw).is_err() {
                std::process::exit(0); // hook 失败绝不能阻塞 Claude Code
            }
            let cfg = config::load_from(&paths::real_config_path());
            let Ok(p) = hook::HookPayload::parse(&raw) else {
                std::process::exit(0);
            };
            let now = chrono::Local::now().timestamp();
            let _ = hook::handle(&p, &paths::real_sessions_dir(), &cfg, now);

            // 计划 9b 的父进程链探针。两条启用方式，理由见 `probe` 顶部：
            //
            // - **未启用**（正常情况）：只有一次环境变量查询加一次 `metadata`，
            //   hook 路径其余一字不变 —— 仍然静默、仍然 `exit 0`。
            // - **安静**（哨兵文件存在）：把链写进 `%APPDATA%\claude-hud\ppid-probe.txt`，
            //   **不弹框** —— 这条路上每个事件都会跑一次探针，模态框会糊满屏幕、
            //   还会挂在 Claude Code 的调用路径上。
            // - **响亮**（设了 `CLAUDE_HUD_DEBUG_PPID`）：写文件**并**弹框。GUI 子系统下
            //   没有控制台可打印，弹框是唯一能让人当场看见的通道（会阻塞到点「确定」，
            //   在显式打开的调试路径上是预期的）。
            // 样本里必须带 `session_id` 与事件名：要判的是"宿主 pid 对**同一个会话**
            // 稳不稳定"，一条不带归属的样本回答不了那个问题（见 `probe` 顶部）。
            let probe_body = probe::ppid_probe(&p.session_id, &p.hook_event_name);
            if probe::loud() {
                if let Some(body) = probe_body {
                    show_message("claude-hud · ppid 探针", &body, false);
                }
            }
            std::process::exit(0);
        }
        Some("--doctor") => {

            // 自检：拿真机数据核一遍那些"靠约定/措辞/路径"的判据还成不成立。

            // 结果走对话框 —— 本进程是 GUI 子系统，`println!` 没有人看得见（见 dialog.rs）。

            let findings = doctor::run(40);

            let bad = findings.iter().filter(|f| f.level == doctor::Level::Bad).count();

            dialog::show_message("claude-hud · 自检", &doctor::render(&findings), bad > 0);

        }

        Some("--install-hooks") => {
            let exe = std::env::current_exe().expect("current_exe");
            let settings = hooks_install::default_settings_path();
            match hooks_install::install(&settings, &exe) {
                Ok(()) => show_message(
                    "claude-hud",
                    &format!("已注册 8 个 hook 到\n{}", settings.display()),
                    false,
                ),
                // 账本第 46 条 / 9c：load() 对「文件存在但读不出来」（含 0 字节、带 BOM）
                // 返回 Err，这里必须把错误**原文**呈现给用户 —— 不能吞掉，也不能当成
                // 意外崩溃。路径一并给出，否则用户不知道该去修哪个文件。
                // 同时以 1 退出：弹框只有人看得见，脚本/父进程只能看退出码，
                // 失败必须对两者都可见（与 UI 分支的 exit(1) 一致）。
                Err(e) => {
                    show_message(
                        "claude-hud · 注册失败",
                        &format!("{}\n\n文件：{}", e, settings.display()),
                        true,
                    );
                    std::process::exit(1);
                }
            }
        }
        Some("--uninstall-hooks") => {
            let exe = std::env::current_exe().expect("current_exe");
            let settings = hooks_install::default_settings_path();
            match hooks_install::uninstall(&settings, &exe) {
                Ok(()) => show_message(
                    "claude-hud",
                    &format!("已移除 hook\n\n文件：{}", settings.display()),
                    false,
                ),
                // 同注册分支：失败必须以退出码 1 呈现给脚本，弹框只覆盖"人"这一半。
                Err(e) => {
                    show_message(
                        "claude-hud · 移除失败",
                        &format!("{}\n\n文件：{}", e, settings.display()),
                        true,
                    );
                    std::process::exit(1);
                }
            }
        }
        _ => {
            // spec §4.3 把"静默回落默认值"点名为本项目最危险的失效模式：配置坏了
            // 就悄悄用 1_000_000，百分比看起来完全合理、不会报错，只会让人在错误的
            // 时机做决策。`ui::run` 内部那次加载是**静默**的（那边本批不动），所以
            // 这里是唯一能让人看见的位置：区分"文件不存在"（首次运行，不是故障）与
            // "存在但读不出来"（故障），后者连原文件一起存档后报给用户。
            if let Some(warn) = config::report_broken(&paths::real_config_path()) {
                show_message("claude-hud · 配置未被读取", &warn, true);
            }
            if let Err(e) = ui::run() {
                // 与 --install-hooks 的失败路径同理：GUI 子系统下没有控制台，
                // `eprintln!` 写进虚空 —— 挂件启动失败时用户会以为"什么都没发生"。
                // 失败必须让人看见。
                show_message(
                    "claude-hud · 挂件启动失败",
                    &format!("{}", e),
                    true,
                );
                std::process::exit(1);
            }
        }
    }
}

// ---- CLI 输出通道 ------------------------------------------------------------
// 顶部那个 `#![windows_subsystem = "windows"]` 让进程没有控制台，于是
// `println!` / `eprintln!` 会写进虚空 —— 用户点开 exe 只会看到什么都没有。
// 所有结果因此必须走系统对话框。
//
// `show_message` 本身已搬到 `dialog.rs`：`ui.rs` 也要用它（中文字体缺失那条），
// 而它原先住在这里，`ui::run` 够不着。

/// 把 panic 从"写 stderr"改成"弹框"。存在的理由、以及为什么跳过 `--hook`，
/// 见 `main` 开头那段。
///
/// `format!("{info}")` 给出的是 `panicked at <文件>:<行>:<列>:` 加 payload —— 位置正是
/// 诊断"无声死亡"最需要的东西。
///
/// ⚠️ 这个 hook **自身不得再 panic**：`show_message` 只是一次 FFI 调用加两段 UTF-16
/// 编码，不含可能 panic 的索引或解析。将来若往这里加逻辑，先确认它不会在 panic 期间
/// 再 panic（那个后果是 abort，连弹框都没有）。
///
/// **与字体校验的分工**：`ui::looks_like_font` 负责把"已知会让 epaint panic 的字节"
/// 挡在门外（更早、更精确）；这里是兜底，覆盖它没能穷尽的所有 panic 来源。
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        show_message("claude-hud · 崩溃", &format!("{info}"), true);
    }));
}
