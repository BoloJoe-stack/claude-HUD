# claude-hud 核心实现计划（M1–M3）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `claude-hud.exe` 能通过 hook 收集多个 Claude Code 会话的状态，并在一个常驻小窗口里把它们实时显示出来。

**Architecture:** 单 exe 双模式——无参数启动 egui 挂件（常驻、轮询文件），`--hook <Event>` 读 stdin JSON 并原子更新状态文件后立即退出。挂件不订阅任何事件通道，只做 1Hz 文件轮询 + transcript 增量 tail；因此挂件可以随时开关而不丢数据。状态机与行视图模型都是纯函数，与 IO 和渲染解耦，从而可被单元测试完整覆盖。

**Tech Stack:** Rust 2021 · `eframe`/`egui`（GUI）· `serde`/`serde_json`（JSON）· `chrono`（本地时间格式化）· ~~`sysinfo`（进程存活检测）~~

> **更正（2026-09-14，合并前收尾批）：`sysinfo` 依赖已删除。** 它自 Task 1 起就写在
> Cargo.toml 里，但**全仓一次都没用到** —— "进程存活检测"（spec §7 坑 2 的僵尸回收）
> 因 9b 的实测结论还没出来而从未实现。僵尸回收的最终方案若走"超时回收"，进程存活检测
> 根本不需要，这个依赖会永远停在"声明了、没读者"的状态（正是本批在
> `model_overrides` / `stale_after_sec` 上清掉的那类缺陷）。9b 的探针改用 Win32
> `CreateToolhelp32Snapshot`（一次快照拿到全进程表的 pid/ppid/exe 名），理由与实现见
> `src/probe.rs` 顶部注释与 9b 一节。真有消费者时再加回来。

**Spec:** `docs/superpowers/specs/2026-09-14-claude-hud-design.md`

## Global Constraints

以下为 spec 的项目级要求，**每个任务的要求都隐含包含本节**：

- **仅 Windows**。不写任何 macOS/Linux 分支，不做跨平台抽象。
- 工具链 `stable-x86_64-pc-windows-msvc`（本机 Rust 1.98.1）。
- **零网络**：不发 HTTP 请求、不起监听端口、不调任何 API、不写数据库。
- **不做 per-tool 埋点**：不注册 `PreToolUse` / `PostToolUse` / `PostToolUseFailure`。工具级信息一律从 transcript 读。
- **hook 事件集固定为 8 个**：`SessionStart`、`UserPromptSubmit`、`Notification`、`PreCompact`、`PostCompact`、`Stop`、`StopFailure`、`SessionEnd`。
- **会话标识只能用 hook payload 的 `session_id`**。禁止用 `$PPID`，禁止用 `CLAUDE_CODE_SESSION_ID` 环境变量（缺失时会兜底成 `"unknown"`，把多个会话串成一个）。
- **`cwd` 只能取自 hook payload**。禁止从 `~/.claude/projects/` 的目录名反推——该目录名是有损编码，中文会被替换成 `-`。
- **上下文上限默认 `1_000_000`，且永远不做自动推断**。transcript 只记 `deepseek-flash`，无任何窗口字段。
- 运行时路径全部在 `%APPDATA%\claude-hud\`：`config.json` 与 `sessions\<session_id>.json`。
- 状态文件写入必须**原子**：先写 `.tmp` 再 `rename`。
- 空闲阈值 `idle_after_sec` 默认 **300 秒**。~~`stale_after_sec` 默认 **0**（0 = 完成的会话一直留在列表里）~~
  > **更正（2026-09-14，合并前收尾批）：`stale_after_sec` 已删除** —— 有文档、无读者
  > 的旋钮（用户设了它没有任何效果，也没有任何提示）。僵尸回收的旋钮等 9b 的实测结论
  > 出来再加。见 spec §8 的修订记录。
- 折叠态聚合优先级：**等你确认 > 出错 > 工作中 > 刚完成 > 待命**。
- "第 N 步"口径：自最近一条真实 user 消息（非 `tool_result`）以来，transcript 中 `tool_use` 块的数量。
- 提交信息末尾必须带 `Co-Authored-By: Claude Code <noreply@anthropic.com>`。

---

## File Structure

创建前先看这张表——它锁定了分解决策，每个文件一个明确职责。

| 文件 | 职责 | 依赖 |
|---|---|---|
| `Cargo.toml` | 依赖与 bin 声明 | — |
| `src/main.rs` | 入口分发：`--hook <Event>` → hook 模式；`--install-hooks`/`--uninstall-hooks` → 配置模式；无参数 → UI | 全部 |
| `src/paths.rs` | `%APPDATA%` 解析与目录拼装。**接受 base 参数**以便测试，不在内部读环境变量 | — |
| `src/config.rs` | `Config` 结构、默认值、读写、部分字段缺省时的合并 | `paths`, `serde` |
| `src/model.rs` | **纯函数**状态机：`(当前状态, 事件) → 新状态`；`State` 枚举 | 无（纯逻辑） |
| `src/state.rs` | `SessionState` 结构、原子写、读、列目录、删除 | `paths`, `serde` |
| `src/hook.rs` | hook 模式：读 stdin → 解析 payload → 走状态机 → 落盘 | `state`, `model`, `paths` |
| `src/hooks_install.rs` | 把 8 个 hook 幂等合并进 `~/.claude/settings.json`，以及反向卸载 | `serde_json` |
| `src/transcript.rs` | 增量 tail：按 offset 读新增字节；**纯函数**解析出 usage / aiTitle / 当前工具 / 打断 / 步数 | `serde_json` |
| `src/view.rs` | **纯函数**行视图模型：`(会话列表, 配置, now) → Vec<Row>`，含计时、上下文%、优先级排序 | `state`, `config`, `transcript` |
| `src/ui.rs` | egui 渲染 `Vec<Row>`；不含任何业务逻辑 | `view` |

**为什么这么切**：`model.rs`、`transcript.rs` 的解析部分、`view.rs` 三处是纯函数，能被单元测试完整覆盖——这是能真正 TDD 的部分。`ui.rs` 只做绘制，不含逻辑，因此不需要单测（由 Task 11 的端到端验收覆盖）。IO 集中在 `state.rs` / `config.rs` / `paths.rs`，用真实临时目录测试。

---

## Task 1: 项目骨架与路径解析

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/paths.rs`

**Interfaces:**
- Consumes: 无（首个任务）
- Produces:
  - `paths::app_dir(base: &Path) -> PathBuf` — 返回 `<base>/claude-hud`
  - `paths::sessions_dir(base: &Path) -> PathBuf` — 返回 `<base>/claude-hud/sessions`
  - `paths::config_path(base: &Path) -> PathBuf` — 返回 `<base>/claude-hud/config.json`
  - `paths::real_app_dir() -> PathBuf` — 从 `%APPDATA%` 解析真实目录（**读环境变量，因此不测**）

- [ ] **Step 1: 初始化 Cargo 项目**

```bash
cd /d/projects/claude-hud
cargo init --name claude-hud
```

- [ ] **Step 2: 用 `cargo add` 添加依赖（让版本解析到当前最新，不手写版本号）**

```bash
cargo add serde --features derive
cargo add serde_json
cargo add chrono --no-default-features --features clock
cargo add sysinfo
cargo add eframe
cargo add egui
```

> **更正（2026-09-14，合并前收尾批）：`cargo add sysinfo` 已撤销**（依赖从
> `Cargo.toml` / `Cargo.lock` 删除）。它从加进来那天起就没有任何调用者，理由见
> Tech Stack 处的更正与 9b 一节。

- [ ] **Step 3: 写失败测试**

写入 `src/paths.rs`（此刻还没有实现函数，测试必然编译失败）：

```rust
use std::path::{Path, PathBuf};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_dir_appends_project_name() {
        let base = Path::new("C:\\base");
        assert_eq!(app_dir(base), PathBuf::from("C:\\base\\claude-hud"));
    }

    #[test]
    fn sessions_dir_nests_under_app_dir() {
        let base = Path::new("C:\\base");
        assert_eq!(
            sessions_dir(base),
            PathBuf::from("C:\\base\\claude-hud\\sessions")
        );
    }

    #[test]
    fn config_path_nests_under_app_dir() {
        let base = Path::new("C:\\base");
        assert_eq!(
            config_path(base),
            PathBuf::from("C:\\base\\claude-hud\\config.json")
        );
    }
}
```

在 `src/main.rs` 顶部加 `mod paths;`，否则测试不会被编译。

- [ ] **Step 4: 运行测试确认失败**

Run: `cargo test paths -- --nocapture`
Expected: 编译失败，报 `cannot find function 'app_dir' in this scope`

- [ ] **Step 5: 写最小实现**

在 `src/paths.rs` 的 `use` 之后、`#[cfg(test)]` 之前插入：

```rust
pub fn app_dir(base: &Path) -> PathBuf {
    base.join("claude-hud")
}

pub fn sessions_dir(base: &Path) -> PathBuf {
    app_dir(base).join("sessions")
}

pub fn config_path(base: &Path) -> PathBuf {
    app_dir(base).join("config.json")
}

/// 真实运行目录。读环境变量，故不单测——由 Task 11 端到端覆盖。
pub fn real_app_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    app_dir(&base)
}
```

- [ ] **Step 6: 运行测试确认通过**

Run: `cargo test paths`
Expected: 3 passed

- [ ] **Step 7: 提交**

```bash
git add Cargo.toml Cargo.lock src/
git commit -m "feat: 项目骨架与 %APPDATA% 路径解析"
```

---

## Task 2: 配置读写

**Files:**
- Create: `src/config.rs`
- Modify: `src/main.rs`（加 `mod config;`）
- Test: `src/config.rs` 内嵌 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `paths::{config_path, real_app_dir}`
- Produces:
  - `config::Config` 结构体，字段：`context_limit: u64`、~~`model_overrides: HashMap<String, u64>`~~、`warn_threshold: u8`、`danger_threshold: u8`、`poll_interval_ms: u64`、`idle_after_sec: u64`、~~`stale_after_sec: u64`~~、~~`opacity: f32`~~、`always_on_top: bool`、`window_pos: [f32; 2]`
    > **更正（2026-09-14，合并前收尾批）：`model_overrides` 与 `stale_after_sec` 已删除**
    > （两者都是有文档、无读者的不兑现旋钮）。理由与"将来要加回时的义务"见 spec §8 的
    > 修订记录；实现里的注释在 `config.rs` 的 `impl Default` 上方。
  - `impl Default for Config`
  - `config::load_from(path: &Path) -> Config` — 文件不存在或字段缺失时用默认值补齐
  - `config::save_to(path: &Path, cfg: &Config) -> std::io::Result<()>`

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn default_context_limit_is_one_million() {
        // spec §4.3：上限必须手配，默认 1M，绝不推断
        assert_eq!(Config::default().context_limit, 1_000_000);
    }

    #[test]
    fn default_idle_threshold_is_300s() {
        let c = Config::default();
        assert_eq!(c.idle_after_sec, 300);
        // 更正（2026-09-14，合并前收尾批）：原为 `assert_eq!(c.stale_after_sec, 0);`
        // —— 该字段已删除（有文档、无读者），实现里的这行断言一并删掉。
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempdir();
        let p = dir.join("nope.json");
        assert_eq!(load_from(&p).context_limit, 1_000_000);
    }

    #[test]
    fn partial_file_keeps_defaults_for_absent_fields() {
        let dir = tempdir();
        let p = dir.join("config.json");
        fs::write(&p, r#"{"context_limit": 128000}"#).unwrap();
        let c = load_from(&p);
        assert_eq!(c.context_limit, 128_000);   // 被覆盖
        assert_eq!(c.idle_after_sec, 300);      // 保持默认
        assert_eq!(c.warn_threshold, 50);
    }

    #[test]
    fn round_trip_preserves_values() {
        let dir = tempdir();
        let p = dir.join("config.json");
        let mut c = Config::default();
        // ⚠️ 这里原先是 `c.opacity = 0.5;` 加对应的断言。`opacity` 已随
        // "不做半透明"一起删除（见下方更正），那两行现已无法编译，故移除。
        c.window_pos = [42.0, 99.0];
        save_to(&p, &c).unwrap();
        let back = load_from(&p);
        assert_eq!(back.window_pos, [42.0, 99.0]);
    }

    fn tempdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "claude-hud-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test config`
Expected: 编译失败，`cannot find struct 'Config'`

- [ ] **Step 3: 写最小实现**

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// spec §4.3：必须手配。transcript 里没有任何窗口字段可推断。
    pub context_limit: u64,
    pub model_overrides: HashMap<String, u64>,   // ← 已删除，见下方更正
    pub warn_threshold: u8,
    pub danger_threshold: u8,
    pub poll_interval_ms: u64,
    pub idle_after_sec: u64,
    pub stale_after_sec: u64,                    // ← 已删除，见下方更正
    pub opacity: f32,                            // ← 已删除，见下方更正
    pub always_on_top: bool,
    pub window_pos: [f32; 2],
}

impl Default for Config {
    fn default() -> Self {
        Self {
            context_limit: 1_000_000,
            model_overrides: HashMap::new(),
            warn_threshold: 50,
            danger_threshold: 75,
            poll_interval_ms: 1000,
            idle_after_sec: 300,
            stale_after_sec: 0,                  // ← 已删除，见下方更正
            opacity: 0.92,                       // ← 已删除，见下方更正
            always_on_top: true,
            window_pos: [100.0, 100.0],
        }
    }
}
```

> **更正（2026-09-14，合并前收尾批）：`model_overrides` 与 `stale_after_sec` 两个字段
> 连同 `use std::collections::HashMap;` 一起删掉了**（上面的代码块保留原样以记录当时的
> 计划，实际实现见 `src/config.rs`）。两者都是"有文档、无读者"的旋钮：用户设了它、
> 没有任何效果、也没有任何提示 —— 与本项目已经抓到并修过一次的 `always_on_top` 是同一
> 类缺陷。`model_overrides` 将来真要按模型分上限时再加，`stale_after_sec` 等 9b 的实测
> 结论出来后按正确形状加，**两次都必须连同读它的代码与测试一起加**。
>
> 顺带：`config.rs` 新增 `report_broken(path) -> Option<String>`（GUI 路径用）：把"文件
> 不存在"（首次运行，不是故障）与"存在但读不出来"（故障）分开 —— 后者连原文件一起存档
> 并报给用户。原先 `load_from` 的 `unwrap_or_default()` 把两者混为一谈，正是 spec §4.3
> 点名的"看起来完全合理、不会报错"的失效模式。`--hook` 路径仍走静默的 `load_from`。

```rust
/// 文件不存在或 JSON 损坏时一律退回默认值——挂件不能因为配置坏了就不启动。
pub fn load_from(path: &Path) -> Config {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Config>(&s).ok())
        .unwrap_or_default()
}

fn tmp_for(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{:?}.tmp",
        std::process::id(),
        std::thread::current().id()
    ));
    target.with_file_name(name)
}

pub fn save_to(path: &Path, cfg: &Config) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cfg).unwrap();
    // 唯一名兄弟路径（config.json → config.json.<pid>.<tid>.tmp），与目标同目录同卷，
    // 保证 rename 是原子的；唯一性保证两个并发写者不会共用同一个 tmp
    let tmp = tmp_for(path);
    std::fs::write(&tmp, json)?;
    // rename 是提交点；失败时必须删掉自己的 tmp —— 唯一名不像固定名那样会被下一次写
    // 覆盖而自愈，留着就会在配置目录里越积越多
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}
```

注意 `#[serde(default)]` 是"部分字段缺省"能成立的关键——它让每个缺失字段回落到 `Default::default()` 的对应值。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test config`
Expected: 5 passed

- [ ] **Step 5: 提交**

```bash
git add src/config.rs src/main.rs
git commit -m "feat: 配置读写，缺省字段回落默认值"
```

---

## Task 3: hook payload 解析

**Files:**
- Create: `src/hook.rs`
- Modify: `src/main.rs`（加 `mod hook;`）

**Interfaces:**
- Consumes: 无
- Produces:
  - `hook::HookPayload` 结构体：公共字段 `session_id: String`、`transcript_path: Option<String>`、`cwd: String`、`hook_event_name: String`、`permission_mode: Option<String>`；`Notification` 专用 `message: Option<String>`、`title: Option<String>`、`notification_type: Option<String>`；`Stop` 专用 `last_assistant_message: Option<String>`；`SessionStart` 专用 `source: Option<String>`
  - `hook::HookPayload::parse(s: &str) -> serde_json::Result<HookPayload>`

- [ ] **Step 1: 写失败测试**

用官方文档里的真实样例 JSON（不是编造的）：

```rust
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
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test hook::`
Expected: 编译失败，`cannot find struct 'HookPayload'`

- [ ] **Step 3: 写最小实现**

```rust
use serde::Deserialize;

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
```

不写 `#[serde(deny_unknown_fields)]`，这样 Claude Code 未来新增字段不会让 hook 崩掉。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test hook::`
Expected: 4 passed

- [ ] **Step 5: 提交**

```bash
git add src/hook.rs src/main.rs
git commit -m "feat: hook payload 解析（宽容未知字段）"
```

---

## Task 4: 状态机（纯函数）

**Files:**
- Create: `src/model.rs`
- Modify: `src/main.rs`（加 `mod model;`）

**Interfaces:**
- Consumes: `hook::HookPayload`、`config::Config`
- Produces:
  - `model::State` 枚举：`Idle`、`Working`、`Waiting`、`Compacting`、`Done`、`Error`、`Interrupted`
  - `model::State::as_str(&self) -> &'static str`
  - `model::transition(current: State, payload: &HookPayload, cfg: &Config, now: i64) -> Transition`，其中 `Transition { state: State, delete: bool }`
  - `model::decay(state: State, state_since: i64, cfg: &Config, now: i64) -> State` — 处理"完成满 `idle_after_sec` 后变待命"

- [ ] **Step 1: 写失败测试**

```rust
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
        // 只有 permission_prompt 才代表"等你确认"
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
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test model::`
Expected: 编译失败，`cannot find enum 'State'`

- [ ] **Step 3: 写最小实现**

```rust
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
```

`Interrupted` 目前没有事件能产生它——它由 Task 9 的 transcript 解析置位。此处先让它参与 decay，Task 9 再补上赋值路径。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test model::`
Expected: 11 passed

- [ ] **Step 5: 提交**

```bash
git add src/model.rs src/main.rs
git commit -m "feat: 纯函数状态机与终态降级"
```

---

## Task 5: 会话状态文件原子读写

**Files:**
- Create: `src/state.rs`
- Modify: `src/main.rs`（加 `mod state;`）

**Interfaces:**
- Consumes: `paths`、`model::State`、`config::Config`
- Produces:
  - `state::SessionState` 结构体，字段：`session_id: String`、`cwd: String`、`transcript_path: Option<String>`、`display_name: Option<String>`、`claude_pid: Option<u32>`、`state: String`、`state_since: i64`、`last_event: String`、`last_event_at: i64`、`last_assistant_message: Option<String>`、`notification_message: Option<String>`、`transcript_offset: u64`
  - `state::save_atomic(sessions_dir: &Path, s: &SessionState) -> io::Result<()>`
  - `state::load_one(sessions_dir: &Path, session_id: &str) -> Option<SessionState>`
  - `state::list_all(sessions_dir: &Path) -> Vec<SessionState>`
  - `state::delete(sessions_dir: &Path, session_id: &str) -> io::Result<()>`

`state` 存为 `String` 而非 `model::State`，理由：状态文件是跨进程契约，序列化成可读短串便于人工排查；同时避免读取未来版本写的未知状态值时报错。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join(format!("claude-hud-state-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn sample(id: &str) -> SessionState {
        SessionState {
            session_id: id.into(),
            cwd: "D:\\projects\\doc-tasks".into(),
            transcript_path: Some("C:\\Users\\user\\.claude\\projects\\x\\y.jsonl".into()),
            display_name: Some("示例项目报告排版修复".into()),
            claude_pid: Some(4242),
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
        // 中文路径必须原样往返（UTF-8 + JSON 转义）
        assert_eq!(back.cwd, "D:\\projects\\doc-tasks");
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
        let d = tempdir("missing").join("does-not-exist");
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
        delete(&d, "gone").unwrap();
        assert!(load_one(&d, "gone").is_none());
    }

    #[test]
    fn delete_missing_file_is_ok() {
        let d = tempdir("delmissing");
        assert!(delete(&d, "never-existed").is_ok());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test state::`
Expected: 编译失败，`cannot find struct 'SessionState'`

- [ ] **Step 3: 写最小实现**

```rust
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

/// 唯一名兄弟路径（<id>.json → <id>.json.<pid>.<tid>.tmp），与目标同目录同卷，
/// 保证 rename 是原子的；唯一性保证两个并发写者不会共用同一个 tmp。
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
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test state::`
Expected: 7 passed

- [ ] **Step 5: 提交**

```bash
git add src/state.rs src/main.rs
git commit -m "feat: 会话状态文件原子读写与容错列表"
```

---

## Task 6: hook 模式接线

**Files:**
- Modify: `src/hook.rs`（追加 `handle` 函数）
- Modify: `src/model.rs`（追加 `State::from_str`）

> **控制器更正（Ruling A）：** 本节原写 "Modify: `src/main.rs`（加参数分发）"，**该行已作废**。参数分发的真实实现在 Task 11 的 Step 4（`main` 的完整参数分发 + 集成测试都在那里）。本任务的五个步骤没有一步动 `main.rs`，原 Files 行与提交步骤属计划撰写时的残留。**本任务不改 `main.rs`**。
>
> 另外，本任务的 `handle` 需要字符串→枚举转换。**不要在本文件里定义局部 `state_from_str`** —— pre-flight 的 Ruling #2 已裁定该转换集中在 `model.rs`，供 T6/T10 共用，消除两者各自实现的逐字重复。见下方 Step 3。

**Interfaces:**
- Consumes: `hook::HookPayload`、`model::{transition, State}`、`state::*`、`paths::sessions_dir`
- Produces:
  - `hook::handle(payload: &HookPayload, sessions_dir: &Path, cfg: &Config, now: i64) -> std::io::Result<()>`
  - `main` 支持：`--hook`（从 stdin 读 payload）、`--install-hooks`、`--uninstall-hooks`、无参数（UI）

**关于事件名**：spec 写的是 `--hook <EventName>`，此处收窄为 `--hook`，事件名从 payload 的 `hook_event_name` 读。理由：payload 里本来就有这个字段，两边都传会多一个可能不同步的来源。这是对 spec 的有意细化，不是遗漏。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod handle_tests {
    use super::*;
    use crate::config::Config;
    use crate::state;

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join(format!("claude-hud-handle-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
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
        // offset 由 Task 9 的挂件侧更新；hook 侧不得把它清零，
        // 否则每来一个事件挂件都要重扫整个 transcript。
        let d = tempdir("offset");
        handle(&payload("SessionStart", None, None), &d, &Config::default(), 1000).unwrap();
        let mut s = state::load_one(&d, "s1").unwrap();
        s.transcript_offset = 524_288;
        state::save_atomic(&d, &s).unwrap();
        handle(&payload("UserPromptSubmit", None, None), &d, &Config::default(), 1100).unwrap();
        assert_eq!(state::load_one(&d, "s1").unwrap().transcript_offset, 524_288);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test hook::handle_tests`
Expected: 编译失败，`cannot find function 'handle'`

- [ ] **Step 3: 写最小实现**

分两半做。

**先做这一半 —— 把字符串→枚举的转换加进 `model.rs`**（pre-flight Ruling #2）。在 `src/model.rs` 的 `impl State` 里、`as_str` 之后加：

```rust
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
```

**再做 `hook.rs` 这一半。** 在 `src/hook.rs` 追加（**不要再定义本地的 `state_from_str`**，用 `model` 里刚加的那个）：

```rust
use crate::config::Config;
use crate::model::{transition, State};
use crate::state::{self, SessionState};
use std::path::Path;

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
        state: State::Idle.as_str().to_string(),
        state_since: now,
        last_event: String::new(),
        last_event_at: now,
        last_assistant_message: None,
        notification_message: None,
        transcript_offset: 0,
    });

    let next = SessionState {
        session_id: p.session_id.clone(),
        cwd: p.cwd.clone(),
        // payload 给了就更新，没给就保留——SessionStart 之后的事件不一定带 transcript_path
        transcript_path: p
            .transcript_path
            .clone()
            .or(prev.transcript_path.clone()),
        display_name: prev.display_name.clone(),
        claude_pid: prev.claude_pid,
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
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test hook::handle_tests`
Expected: 7 passed

- [ ] **Step 5: 提交**

```bash
git add src/hook.rs src/model.rs
git commit -m "feat: hook 模式接线，state_since 仅在状态跃迁时重置"
```

---

## Task 7: hooks 安装 / 卸载（幂等合并 settings.json）

**Files:**
- Create: `src/hooks_install.rs`
- Modify: `src/main.rs`（加 `mod hooks_install;`）

**Interfaces:**
- Consumes: `serde_json`
- Produces:
  - `hooks_install::EVENTS: [&str; 8]`
  - `hooks_install::install(settings_path: &Path, exe: &Path) -> std::io::Result<()>`
  - `hooks_install::uninstall(settings_path: &Path, exe: &Path) -> std::io::Result<()>`
  - `hooks_install::default_settings_path() -> PathBuf` — `~/.claude/settings.json`

> ⚠️ **测试只准碰 fixture 路径。** 这个模块会改写用户全局配置，跑测试时若误传到真实 `~/.claude/settings.json`，会让本会话和所有其他会话的每个事件都去 spawn 一个还没写完的 exe。**运行 `--install-hooks` 前必须先征得用户明确同意**（见 Task 11 验收清单）。

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("claude-hud-install-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn exe() -> PathBuf {
        PathBuf::from("C:\\tools\\claude-hud.exe")
    }

    fn read(p: &PathBuf) -> Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    fn our_entries(v: &Value, exe: &PathBuf) -> usize {
        EVENTS
            .iter()
            .filter_map(|e| v.pointer(&format!("/hooks/{}", e)))
            .filter_map(|g| g.as_array())
            .flatten()
            .filter_map(|grp| grp.pointer("/hooks"))
            .filter_map(|h| h.as_array())
            .flatten()
            .filter(|h| h.get("command").and_then(|c| c.as_str()) == Some(&exe.to_string_lossy()))
            .count()
    }

    #[test]
    fn install_creates_all_eight_events() {
        let d = tempdir("create");
        let p = d.join("settings.json");
        install(&p, &exe()).unwrap();
        let v = read(&p);
        for e in EVENTS {
            assert!(v.pointer(&format!("/hooks/{}", e)).is_some(), "缺 {}", e);
        }
        assert_eq!(our_entries(&v, &exe()), 8);
    }

    #[test]
    fn install_uses_exec_form_without_shell() {
        let d = tempdir("execform");
        let p = d.join("settings.json");
        install(&p, &exe()).unwrap();
        let v = read(&p);
        let h = &v["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(h["type"], "command");
        assert_eq!(h["args"][0], "--hook");
        assert_eq!(h["async"], true);
    }

    #[test]
    fn install_preserves_unrelated_settings() {
        let d = tempdir("preserve");
        let p = d.join("settings.json");
        std::fs::write(&p, r#"{"env":{"FOO":"bar"},"theme":"light"}"#).unwrap();
        install(&p, &exe()).unwrap();
        let v = read(&p);
        assert_eq!(v["env"]["FOO"], "bar", "不能碰用户已有的 env 块");
        assert_eq!(v["theme"], "light");
    }

    #[test]
    fn install_is_idempotent() {
        let d = tempdir("idem");
        let p = d.join("settings.json");
        install(&p, &exe()).unwrap();
        install(&p, &exe()).unwrap();
        install(&p, &exe()).unwrap();
        assert_eq!(our_entries(&read(&p), &exe()), 8, "重复安装不得翻倍");
    }

    #[test]
    fn install_keeps_foreign_hooks() {
        let d = tempdir("foreign");
        let p = d.join("settings.json");
        std::fs::write(
            &p,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"C:\\other.exe"}]}]}}"#,
        )
        .unwrap();
        install(&p, &exe()).unwrap();
        let v = read(&p);
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        assert!(stop.len() >= 2, "别人的 hook 必须留着");
        assert_eq!(our_entries(&v, &exe()), 8);
    }

    #[test]
    fn uninstall_removes_only_ours() {
        let d = tempdir("uninstall");
        let p = d.join("settings.json");
        std::fs::write(
            &p,
            r#"{"env":{"FOO":"bar"},"hooks":{"Stop":[{"hooks":[{"type":"command","command":"C:\\other.exe"}]}]}}"#,
        )
        .unwrap();
        install(&p, &exe()).unwrap();
        uninstall(&p, &exe()).unwrap();
        let v = read(&p);
        assert_eq!(our_entries(&v, &exe()), 0);
        assert_eq!(v["env"]["FOO"], "bar");
        assert!(v["hooks"]["Stop"].as_array().unwrap().iter().any(|g| {
            g.pointer("/hooks/0/command").and_then(|c| c.as_str())
                == Some("C:\\other.exe")
        }));
    }

    #[test]
    fn uninstall_on_clean_file_is_noop() {
        let d = tempdir("cleanuninstall");
        let p = d.join("settings.json");
        std::fs::write(&p, r#"{"env":{"FOO":"bar"}}"#).unwrap();
        uninstall(&p, &exe()).unwrap();
        assert_eq!(read(&p)["env"]["FOO"], "bar");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test hooks_install::`
Expected: 编译失败，`cannot find function 'install'`

- [ ] **Step 3: 写最小实现**

```rust
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const EVENTS: [&str; 8] = [
    "SessionStart",
    "UserPromptSubmit",
    "Notification",
    "PreCompact",
    "PostCompact",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

pub fn default_settings_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude").join("settings.json")
}

fn exe_str(exe: &Path) -> String {
    exe.to_string_lossy().to_string()
}

/// exec 形式：command 是 exe 绝对路径、args 是参数向量。
/// 这样不经过 shell，既没有引号转义问题，启动也最快。
fn our_group(exe: &Path) -> Value {
    json!([{
        "hooks": [{
            "type": "command",
            "command": exe_str(exe),
            "args": ["--hook"],
            "async": true
        }]
    }])
}

fn is_our_group(grp: &Value, exe: &Path) -> bool {
    grp.pointer("/hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|h| {
                h.get("command").and_then(|c| c.as_str()) == Some(&exe_str(exe))
            })
        })
}

fn load(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

fn store(path: &Path, v: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(v).unwrap())
}

pub fn install(settings_path: &Path, exe: &Path) -> std::io::Result<()> {
    let mut root = load(settings_path);

    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();

    for event in EVENTS {
        let slot = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        if !slot.is_array() {
            *slot = json!([]);
        }
        let arr = slot.as_array_mut().unwrap();
        if arr.iter().any(|g| is_our_group(g, exe)) {
            continue; // 幂等：已经装过就不重复加
        }
        for g in our_group(exe).as_array().unwrap() {
            arr.push(g.clone());
        }
    }

    store(settings_path, &root)
}

pub fn uninstall(settings_path: &Path, exe: &Path) -> std::io::Result<()> {
    let mut root = load(settings_path);
    let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return store(settings_path, &root);
    };

    for event in EVENTS {
        if let Some(arr) = hooks.get_mut(event).and_then(|s| s.as_array_mut()) {
            arr.retain(|g| !is_our_group(g, exe));
            if arr.is_empty() {
                hooks.remove(event);
            }
        }
    }

    store(settings_path, &root)
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test hooks_install::`
Expected: 7 passed

- [ ] **Step 5 / Step 6: 真机验证父进程链 —— ⚠️ 已由控制器整体移出本任务，改在 Task 11 执行**

> **控制器裁定（Ruling B）：** 原 Step 5 要求"在 `main.rs` 的 `--hook` 分支加调试输出，然后触发一次真实对话"，原 Step 6 要求据此裁决"保留 `claude_pid` 还是改用 `zombie_after_sec`"。**这两步在本任务执行不了**，原因是两条硬约束：
>
> 1. **`--hook` 分支此刻不存在。** Ruling A 已裁定 `main.rs` 的参数分发整体在 Task 11 落地；本任务只加 `mod hooks_install;`。没有分支可加调试输出。
> 2. **触发真实 hook 需要先把 hook 注册进全局 `~/.claude/settings.json`** —— 这是**外部副作用**，skill 明列为必须停下征得用户同意的四类之一，而本计划的既定停点正是 Task 11 的 Step 9。
>
> 因此父进程链探针与随后的裁决**整体移交 Task 11**，与其已有的验收环节合并执行。
>
> **已记账的后果：** `SessionState.claude_pid` 在 Task 11 之前**始终为 `None`**（Task 6 只做透传，无人写入）。**Task 10 不得依赖它做存活检测。** 僵尸回收的实际方案在 Task 11 定。
>
> **本任务因此只剩 Step 1–4 与 Step 7。**

- [ ] **Step 7: 提交**

```bash
git add src/hooks_install.rs src/main.rs
git commit -m "feat: hooks 幂等安装/卸载，exec 形式不过 shell"
```

---

## Task 8: transcript 增量 tail

**Files:**
- Create: `src/transcript.rs`
- Modify: `src/main.rs`（加 `mod transcript;`）

**Interfaces:**
- Consumes: 无
- Produces:
  - `transcript::read_new(path: &Path, offset: u64) -> std::io::Result<Option<(String, u64)>>`
    - 返回 `None` 表示文件不存在
    - 返回 `Some((新增文本, 新 offset))`；若检测到文件比 offset 短（被截断/轮换），从 0 重读

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod read_tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("claude-hud-tail-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
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
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test transcript::read_tests`
Expected: 编译失败，`cannot find function 'read_new'`

- [ ] **Step 3: 写最小实现**

```rust
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
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test transcript::read_tests`
Expected: 7 passed

> **更正：** 本行原写 "6 passed"，但 Step 1 的 `mod read_tests` 里实际有 7 个 `#[test]`（含自审时补入的 `partial_line_is_not_consumed`）。以测试代码为准 —— T8 的实现者也正是这么判断的。

- [ ] **Step 5: 提交**

```bash
git add src/transcript.rs src/main.rs
git commit -m "feat: transcript 增量 tail，按字节记账并按需回退重读"
```

---

## Task 9: transcript 解析（纯函数）

**Files:**
- Modify: `src/transcript.rs`（追加解析部分）

**Interfaces:**
- Consumes: `serde_json`
- Produces:
  - `transcript::Delta`（`Default` + `Clone` + `PartialEq`）：`context_tokens: Option<u64>`、`ai_title: Option<String>`、`last_tool: Option<String>`、`last_tool_detail: Option<String>`、`interrupted: bool`、`step_count: u32`
  - `transcript::parse_delta(text: &str, prior: &Delta) -> Delta`
  - `transcript::summarize_tool(name: &str, input: &serde_json::Value) -> Option<String>`

**解析口径（全部来自 spec §4.2 的实测字段）**

| 目标 | 判定 |
|---|---|
| 上下文 token | `type=="assistant"` 且 `message.usage` 存在 → `input_tokens + cache_read_input_tokens + cache_creation_input_tokens` |
| 会话名 | `type=="ai-title"` → `aiTitle` |
| 当前工具 | `message.content[]` 中 `type=="tool_use"` → `name` |
| 步数 | 每个 `tool_use` 块 +1；遇到**真实 user 消息**归零 |
| 真实 user 消息 | `type=="user"` 且其 `message.content` 中**不含** `tool_result` 块 |
| 被打断 | 任意行含 `interruptedMessageId` |

- [ ] **Step 1: 写失败测试**

```rust
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
            ]}
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
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test transcript::parse_tests`
Expected: 编译失败，`cannot find struct 'Delta'`

- [ ] **Step 3: 写最小实现**

在 `src/transcript.rs` 追加：

```rust
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Delta {
    pub context_tokens: Option<u64>,
    pub ai_title: Option<String>,
    pub last_tool: Option<String>,
    pub last_tool_detail: Option<String>,
    pub interrupted: bool,
    pub step_count: u32,
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

        if v.get("interruptedMessageId").is_some() {
            d.interrupted = true;
        }

        match v.get("type").and_then(|t| t.as_str()) {
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
                    d.ai_title = Some(t.to_string());
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
                // 带 tool_result 的 user 行是"工具回执"，不是新一轮
                let is_tool_result_carrier = content.is_some_and(content_has_tool_result);
                if !is_tool_result_carrier {
                    d.step_count = 0;
                }
            }
            _ => {}
        }
    }

    d
}
```

注意 `content` 为纯字符串（真实用户发言）时 `content_has_tool_result` 返回 `false`，因此会归零——这正是我们要的。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test transcript::parse_tests`
Expected: 10 passed

- [ ] **Step 5: 提交**

```bash
git add src/transcript.rs
git commit -m "feat: transcript 纯函数解析（usage/标题/工具/步数/打断）"
```

---

## Task 10: 行视图模型（纯函数）

**Files:**
- Create: `src/view.rs`
- Modify: `src/main.rs`（加 `mod view;`）

**Interfaces:**
- Consumes: `state::SessionState`、`config::Config`、`model::{State, decay}`
- Produces:
  - `view::Progress`：`Tasks { done: u32, total: u32 }` | `Steps(u32)` | `None`
  - `view::Row`：`session_id: String`、`name: String`、`state: State`、`elapsed_secs: i64`、`detail: Option<String>`、`context_tokens: Option<u64>`、`context_limit: u64`、`context_pct: Option<f32>`、`progress: Progress`、`subtitle: Option<String>`
  - `view::build_rows(states: &[SessionState], cfg: &Config, now: i64) -> Vec<Row>`
  - `view::aggregate(rows: &[Row]) -> Option<State>` — 折叠态用的聚合状态

**排序优先级**（spec §2，折叠聚合也用同一顺序）：等你确认 `Waiting` > 出错 `Error` > 工作中 `Working` > 压缩中 `Compacting` > 刚完成 `Done` / 被打断 `Interrupted` > 待命 `Idle`。

- [ ] **Step 1: 写失败测试**

```rust
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
            state: state.into(),
            state_since: since,
            last_event: "X".into(),
            last_event_at: last_at,
            last_assistant_message: None,
            notification_message: None,
            transcript_offset: 0,
        }
    }

    #[test]
    fn rows_are_sorted_by_urgency() {
        let states = vec![
            st("a", "idle", 0, 10),
            st("b", "working", 0, 20),
            st("c", "waiting", 0, 30),
            st("d", "error", 0, 40),
        ];
        let rows = build_rows(&states, &Config::default(), 100);
        let order: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(order, vec!["c", "d", "b", "a"], "等待 > 出错 > 工作中 > 待命");
    }

    #[test]
    fn same_priority_sorted_by_recency() {
        let states = vec![st("old", "working", 0, 10), st("new", "working", 0, 90)];
        let rows = build_rows(&states, &Config::default(), 100);
        assert_eq!(rows[0].session_id, "new", "同级取最近有活动的");
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
        let states = vec![st("a", "idle", 0, 1), st("b", "working", 0, 2), st("c", "waiting", 0, 3)];
        let rows = build_rows(&states, &Config::default(), 100);
        assert_eq!(aggregate(&rows), Some(State::Waiting));
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
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test view::`
Expected: 编译失败，`cannot find function 'build_rows'`

- [ ] **Step 3: 写最小实现**

```rust
use crate::config::Config;
use crate::model::{decay, State};
use crate::state::SessionState;

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Tasks { done: u32, total: u32 },
    Steps(u32),
    None,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub session_id: String,
    pub name: String,
    pub state: State,
    pub elapsed_secs: i64,
    pub detail: Option<String>,
    pub context_tokens: Option<u64>,
    pub context_limit: u64,
    pub context_pct: Option<f32>,
    pub progress: Progress,
    pub subtitle: Option<String>,
}

/// 越小越"需要你"。折叠态聚合与列表排序共用这一个顺序。
fn priority(s: State) -> u8 {
    match s {
        State::Waiting => 0,
        State::Error => 1,
        State::Working => 2,
        State::Compacting => 3,
        State::Done => 4,
        State::Interrupted => 4,
        State::Idle => 5,
    }
}

// 字符串→枚举的转换由 model::State::from_str 提供（Ruling #2：集中定义在
// model.rs，供 Task 6 与 Task 10 共用，不要在此重复实现）。

fn basename(p: &str) -> String {
    p.replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(p)
        .to_string()
}

pub fn build_rows(states: &[SessionState], cfg: &Config, now: i64) -> Vec<Row> {
    let mut keyed: Vec<(Row, i64)> = states
        .iter()
        .map(|s| {
            let raw = State::from_str(&s.state);
            let state = decay(raw, s.state_since, cfg, now);

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

            // 上下文 token 这里从会话状态读不到，故留空。**填充它的是 Task 12 的
            // `build_rows_with_deltas`**（从 `transcript::Delta` 取值），不是 Task 11 ——
            // 本行原写"由调用方在 Task 11 里注入"，那是错的措辞，已更正。
            // Task 11 的 `ui.rs` 先用这个留空版本，Task 12 会把它换成 `build_rows_with_deltas`。
            let context_tokens: Option<u64> = None;
            let context_pct = context_tokens
                .map(|t| (t as f64 / cfg.context_limit.max(1) as f64) as f32);

            let subtitle = match state {
                State::Done | State::Error | State::Interrupted => s
                    .last_assistant_message
                    .clone()
                    .or_else(|| s.notification_message.clone()),
                State::Waiting => s.notification_message.clone(),
                _ => None,
            };

            (
                Row {
                    session_id: s.session_id.clone(),
                    name,
                    state,
                    elapsed_secs: (now - s.state_since).max(0),
                    detail: None,
                    context_tokens,
                    context_limit: cfg.context_limit,
                    context_pct,
                    progress: Progress::None,
                    subtitle,
                },
                // 次级排序键。`Row` 的接口里没有它，故不能进 `Row`；
                // 但 spec §9 要求「同级取最近有**事件**的那个」，必须带上。
                s.last_event_at,
            )
        })
        .collect();

    // ⚠️ 次级键是 `last_event_at` 降序，**不是** `elapsed_secs` 降序。
    // `elapsed_secs` 量的是"进入当前状态多久"：两个同级且 `state_since` 相同的会话，
    // 它的值完全相同 → 次级键恒等 → 稳定排序保留输入顺序，与 spec §9 不符。
    // Task 10 的实现者靠真跑测试发现此点（9 passed / 1 FAILED），做了最小修正。
    keyed.sort_by(|(a, a_at), (b, b_at)| {
        priority(a.state)
            .cmp(&priority(b.state))
            .then_with(|| b_at.cmp(a_at))
    });

    keyed.into_iter().map(|(row, _)| row).collect()
}

pub fn aggregate(rows: &[Row]) -> Option<State> {
    rows.iter().map(|r| r.state).min_by_key(|s| priority(*s))
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test view::`
Expected: 10 passed

> **更正：** 本行原写 "11 passed"。pre-flight 扫描裁定删除空测试 `context_pct_divides_by_configured_limit` 后，本任务剩 10 个用例 —— 但我只删了测试、忘了改这个数字。（同类错误在 Task 8 也出现过一次。）

- [ ] **Step 5: 提交**

```bash
git add src/view.rs src/main.rs
git commit -m "feat: 纯函数行视图模型与聚合优先级"
```

---

## Task 11: egui 渲染与端到端验收

**Files:**
- Create: `src/ui.rs`
- Create: `tests/hook_e2e.rs`
- Modify: `src/main.rs`（参数分发 + 轮询循环）
- Modify: `src/paths.rs`（加 `CLAUDE_HUD_SESSIONS_DIR` 环境变量覆盖，供集成测试用）

**Interfaces:**
- Consumes: `view::{build_rows, Row, Progress}`、`state::*`、`config::*`、`transcript::*`
- Produces: 可运行的 `claude-hud.exe`

- [ ] **Step 1: 写集成测试（真实进程 + 真实文件）**

`tests/hook_e2e.rs`：

```rust
use std::io::Write;
use std::process::{Command, Stdio};

fn tempdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("claude-hud-e2e-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run_hook(sessions_dir: &std::path::Path, payload: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_claude-hud"))
        .arg("--hook")
        .env("CLAUDE_HUD_SESSIONS_DIR", sessions_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn claude-hud --hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "hook 必须以 0 退出，stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn full_lifecycle_writes_then_removes_state_file() {
    let d = tempdir("lifecycle");

    run_hook(
        &d,
        r#"{"session_id":"e2e-1","cwd":"D:\\w","hook_event_name":"SessionStart","source":"startup"}"#,
    );
    let f = d.join("e2e-1.json");
    assert!(f.exists(), "SessionStart 后应出现状态文件");

    run_hook(
        &d,
        r#"{"session_id":"e2e-1","cwd":"D:\\w","hook_event_name":"UserPromptSubmit"}"#,
    );
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
    assert_eq!(v["state"], "working");

    // 中文必须原样往返（真实环境里 cwd 全是中文路径）
    run_hook(
        &d,
        r#"{"session_id":"e2e-1","cwd":"D:\\projects\\doc-tasks","hook_event_name":"Stop","last_assistant_message":"已修复排版，共 12 处"}"#,
    );
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
    assert_eq!(v["state"], "done");
    assert_eq!(v["cwd"], "D:\\projects\\doc-tasks");
    assert_eq!(v["last_assistant_message"], "已修复排版，共 12 处");

    run_hook(
        &d,
        r#"{"session_id":"e2e-1","cwd":"D:\\w","hook_event_name":"SessionEnd"}"#,
    );
    assert!(!f.exists(), "SessionEnd 后状态文件必须消失");
}

#[test]
fn two_sessions_do_not_collide() {
    let d = tempdir("two");
    run_hook(&d, r#"{"session_id":"a","cwd":"D:\\wa","hook_event_name":"UserPromptSubmit"}"#);
    run_hook(&d, r#"{"session_id":"b","cwd":"D:\\wb","hook_event_name":"UserPromptSubmit"}"#);
    assert!(d.join("a.json").exists());
    assert!(d.join("b.json").exists());
    let a: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(d.join("a.json")).unwrap()).unwrap();
    assert_eq!(a["cwd"], "D:\\wa", "两个会话的 cwd 不能串");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --test hook_e2e`
Expected: 失败——`--hook` 分支尚未在 `main.rs` 实现，进程要么没输出要么非 0 退出

- [ ] **Step 3: 加环境变量覆盖（`src/paths.rs`）**

```rust
/// `%APPDATA%` 的 **base**（**不是** app 目录）。
/// `real_app_dir()` 返回 app 目录，而 `sessions_dir` / `config_path` 收的是 base ——
/// 把前者喂给后者会得到 `…\claude-hud\claude-hud\…`。这个命名陷阱是 Task 1 评审记下的。
fn real_base() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 集成测试用：允许用环境变量把状态目录指到临时目录。
/// 生产路径不受影响——不设这个变量时行为与之前完全一致。
pub fn real_sessions_dir() -> PathBuf {
    if let Some(over) = std::env::var_os("CLAUDE_HUD_SESSIONS_DIR") {
        return PathBuf::from(over);
    }
    sessions_dir(&real_base())
}

/// 同一个道理，配置路径也需要可覆盖的真实入口。两个理由：
/// 1. **集成测试不得读写用户真实的 `%APPDATA%\claude-hud\config.json`** ——
///    没有这个覆盖，测试会依赖机器上的真实配置，不 hermetic。
/// 2. pre-flight 裁定要求本任务**改用 `paths::config_path`**，消除此前两处内联
///    `.join("config.json")` 的重复（扫描已把它记为重复项）。
pub fn real_config_path() -> PathBuf {
    if let Some(over) = std::env::var_os("CLAUDE_HUD_CONFIG_PATH") {
        return PathBuf::from(over);
    }
    config_path(&real_base())
}
```

- [ ] **Step 4: 实现 `main.rs` 参数分发**

```rust
mod config;
mod hook;
mod hooks_install;
mod model;
mod paths;
mod state;
mod transcript;
mod ui;
mod view;

use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

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
            std::process::exit(0);
        }
        Some("--install-hooks") => {
            let exe = std::env::current_exe().expect("current_exe");
            match hooks_install::install(&hooks_install::default_settings_path(), &exe) {
                Ok(()) => println!("已注册 8 个 hook 到 {:?}", hooks_install::default_settings_path()),
                Err(e) => eprintln!("注册失败: {}", e),
            }
        }
        Some("--uninstall-hooks") => {
            let exe = std::env::current_exe().expect("current_exe");
            match hooks_install::uninstall(&hooks_install::default_settings_path(), &exe) {
                Ok(()) => println!("已移除 hook"),
                Err(e) => eprintln!("移除失败: {}", e),
            }
        }
        _ => {
            if let Err(e) = ui::run() {
                eprintln!("挂件启动失败: {}", e);
                std::process::exit(1);
            }
        }
    }
}
```

- [ ] **Step 5: 实现 `src/ui.rs` 最小可跑版本**

只做"能看见"：无边框、置顶、每会话一行、状态色条、计时。**外观打磨（圆角、呼吸动画、胶囊折叠、右键菜单、位置记忆）属 M4，不在本计划内。**

> **已撤销的要求：窗口半透明。** 用户 2026-09-14 裁定不需要（这个不重要）。故 `ViewportBuilder` **不加** `with_transparent(true)`；面板颜色改由 `CentralPanel` 的 `panel_frame()` 全权决定（**纯白不透明**）。
>
> ⚠️ **本段原来接着写"`config` 里的 `opacity` 保留 —— 它控制面板填充色的 alpha、且被 `ui.rs` 真实读取，不是死配置项"。那个论断已被推翻：** 不透明白底之后 `opacity` 就失去意义，已从 `Config` **删除**（见下方更正）。留这段话等于留着一份"该把它加回来"的论证 —— 将来做 M4 的人读到它会走错方向。

```rust
use crate::config;
use crate::paths;
use crate::state;
use crate::view::{self, Progress};
use crate::model::State;
use std::time::{Duration, Instant};

pub fn run() -> eframe::Result<()> {
    let cfg = config::load_from(&paths::real_config_path());
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([320.0, 240.0])
            .with_decorations(false)
            .with_always_on_top(),
        ..Default::default()
    };

    eframe::run_native(
        "claude-hud",
        opts,
        Box::new(move |_cc| Ok(Box::new(App::new(cfg)))),
    )
}

struct App {
    cfg: config::Config,
    rows: Vec<view::Row>,
    last_poll: Instant,
}

impl App {
    fn new(cfg: config::Config) -> Self {
        Self { cfg, rows: Vec::new(), last_poll: Instant::now() - Duration::from_secs(60) }
    }

    fn poll(&mut self) {
        let states = state::list_all(&paths::real_sessions_dir());
        let now = chrono::Local::now().timestamp();
        self.rows = view::build_rows(&states, &self.cfg, now);
        self.last_poll = Instant::now();
    }
}

fn state_color(s: State) -> egui::Color32 {
    match s {
        State::Working => egui::Color32::from_rgb(0x3f, 0xb9, 0x50),
        State::Waiting => egui::Color32::from_rgb(0xe3, 0xb3, 0x41),
        State::Error => egui::Color32::from_rgb(0xd9, 0x53, 0x4f),
        State::Compacting => egui::Color32::from_rgb(0x4a, 0x9e, 0xd8),
        State::Done => egui::Color32::from_rgb(0x4a, 0x9e, 0xd8),
        State::Interrupted => egui::Color32::from_rgb(0x9a, 0x9a, 0x9a),
        State::Idle => egui::Color32::from_rgb(0x6a, 0x6a, 0x6a),
    }
}

fn state_label(s: State) -> &'static str {
    match s {
        State::Working => "工作中",
        State::Waiting => "等你确认",
        State::Error => "出错",
        State::Compacting => "压缩中",
        State::Done => "完成",
        State::Interrupted => "被打断",
        State::Idle => "待命",
    }
}

fn fmt_elapsed(secs: i64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.last_poll.elapsed() >= Duration::from_millis(self.cfg.poll_interval_ms) {
            self.poll();
        }
        ctx.request_repaint_after(Duration::from_millis(self.cfg.poll_interval_ms));

        egui::CentralPanel::default().frame(egui::Frame::none().fill(
            // ⚠️ 已过时：原为
            //   `egui::Color32::from_rgba_unmultiplied(0x1e, 0x1e, 0x1e, (self.cfg.opacity * 255.0) as u8)`
            // （深灰半透明底）。用户 2026-09-14 裁定"不做半透明 + 白底黑边"后，`opacity`
            // 已删、底色已换纯白，**原来那一行现在无法编译**。这里给出当前形态。
            egui::Color32::WHITE,
        ))
        .show(ctx, |ui| {
            ui.label(
                egui::RichText::new(format!("● Claude 会话 · {} 个", self.rows.len()))
                    .color(egui::Color32::LIGHT_GRAY)
                    .size(13.0),
            );
            ui.separator();

            if self.rows.is_empty() {
                ui.label(egui::RichText::new("没有活跃会话").color(egui::Color32::GRAY));
                return;
            }

            for row in &self.rows {
                ui.horizontal(|ui| {
                    ui.colored_label(state_color(row.state), "▍");
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&row.name).strong());
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} {}",
                                    state_label(row.state),
                                    fmt_elapsed(row.elapsed_secs)
                                ))
                                .color(state_color(row.state))
                                .size(12.0),
                            );
                        });
                        if let Some(d) = &row.detail {
                            ui.label(egui::RichText::new(d).size(11.0).color(egui::Color32::GRAY));
                        }
                        if let Some(pct) = row.context_pct {
                            let color = if pct * 100.0 >= self.cfg.danger_threshold as f32 {
                                egui::Color32::from_rgb(0xd9, 0x53, 0x4f)
                            } else if pct * 100.0 >= self.cfg.warn_threshold as f32 {
                                egui::Color32::from_rgb(0xe3, 0xb3, 0x41)
                            } else {
                                egui::Color32::GRAY
                            };
                            ui.label(
                                egui::RichText::new(format!(
                                    "上下文 {}%",
                                    (pct * 100.0).round() as i32
                                ))
                                .size(11.0)
                                .color(color),
                            );
                        }
                        match &row.progress {
                            Progress::Tasks { done, total } => {
                                ui.label(
                                    egui::RichText::new(format!("任务 {}/{}", done, total))
                                        .size(11.0)
                                        .color(egui::Color32::GRAY),
                                );
                            }
                            Progress::Steps(n) => {
                                ui.label(
                                    egui::RichText::new(format!("第 {} 步", n))
                                        .size(11.0)
                                        .color(egui::Color32::GRAY),
                                );
                            }
                            Progress::None => {}
                        }
                        if let Some(s) = &row.subtitle {
                            ui.label(egui::RichText::new(s).size(11.0).italics().color(egui::Color32::GRAY));
                        }
                    });
                });
                ui.separator();
            }
        });
    }
}
```

- [ ] **Step 6: 运行集成测试确认通过**

Run: `cargo test --test hook_e2e`
Expected: 2 passed

- [ ] **Step 7: 跑全量测试**

Run: `cargo test`
Expected: 全部通过（Task 1–10 共 60 余个单测 + 2 个集成测试），0 failed

- [ ] **Step 8: 构建 release**

Run: `cargo build --release`
Expected: 成功产出 `target/release/claude-hud.exe`

- [ ] **Step 9: 端到端人工验收（**注册 hook 前必须先问用户**）**

⚠️ `--install-hooks` 会改写全局 `~/.claude/settings.json`，影响**所有**会话（包括当前这个）。执行前必须明确征得用户同意，验收完要能一键 `--uninstall-hooks` 回滚。

**9a. 注册前的三条强制动作（来自 Task 7 评审，SDD 账本第 43/45/46 条）**

- [ ] **先备份** `~/.claude/settings.json`，并把**备份路径明确告知用户**
- [ ] 记录注册前的文件 mtime 与字节数，作为"未被意外改动"的基线
- [ ] 向用户说明：**`--uninstall-hooks` 在原本没有 settings.json 的机器上会创建一个 `{}`**，而不是回到"文件不存在"

**9b. 父进程链探针（Ruling B：原 Task 7 的 Step 5/6 整体移入此处）**

Task 7 无法执行这两步 —— 那时 `--hook` 分支还不存在（Ruling A），且注册 hook 是外部副作用。现在两个条件都具备了。

用 `CLAUDE_HUD_DEBUG_PPID=1` 触发一次真实 hook，观察 hook 进程的父进程链：

- 若父进程链能稳定指到 Claude Code 宿主 → **保留** `SessionState.claude_pid`，并在 `hook.rs` 里真实写入它，供挂件做存活检测。
- 若父进程是某个中间 shell、无法稳定识别 → **删除** `claude_pid` 字段，改用退化方案：`state` 为终态且 `last_event_at` 超过 12 小时才回收，并在 `config.rs` 增加 `zombie_after_sec: 43200`。

**把结论写进代码注释，供后续实现者读到。** 无论哪种结论，都要在报告里写明依据。

> **探针已实现（2026-09-14，合并前收尾批）；上一条"结论"仍待真机测得。**
>
> 原文说"用 `CLAUDE_HUD_DEBUG_PPID=1` 触发一次真实 hook" —— 但**代码里根本不存在这个
> 环境变量**（全仓零匹配），所以按原文执行会**静默什么都不做**，然后给出一个假结论
> "测不出来"；而 GUI 子系统下没有控制台，`println!` 只会写进虚空。本批把探针做出来：
>
> - 实现：`src/probe.rs`，由 `main.rs` 的 `--hook` 分支在 `hook::handle` 之后调用。
> - 触发：`CLAUDE_HUD_DEBUG_PPID` **设了且不为 `0`** 即生效（严格只认 `"1"` 会让
>   `=true` 静默不生效 —— 那正是本节要消灭的失效模式）。
> - 输出：父进程链（**进程名 + pid**，由近到远，最多 32 层，含环保护）写进
>   `%APPDATA%\claude-hud\ppid-probe.txt`；同时用 MessageBox 显示**正文与文件路径**
>   （弹框是这条路径上唯一能让人看见的通道；它会阻塞到点「确定」，这在显式打开探针的
>   调试路径上是预期的）。
> - 纪律：未设该变量时正常 hook 路径**一字不变** —— 仍然静默、仍然 `exit 0`。
> - 父进程链的取法用 Win32 `CreateToolhelp32Snapshot` 而**不是** `sysinfo`（依赖已删），
>   理由见 `src/probe.rs` 顶部注释与本文 Tech Stack 处的更正。
>
> **真机实测还缺一步**：hook 是 **Claude Code spawn 的**，探针只认 hook 进程自己的环境
> 变量，所以必须在 **Claude Code 启动时**就把 `CLAUDE_HUD_DEBUG_PPID` 放进它的环境
> （例如 `set CLAUDE_HUD_DEBUG_PPID=1 && claude`），然后随便触发一个事件。
> 本批已在本机验证探针本身可用（经 `cmd.exe` 起 hook 时链为
> `cmd.exe → bash.exe ×4 → claude.exe → powershell.exe → Code.exe`），
> 但**"Claude Code 直接 spawn hook 时的直接父进程是谁"仍未测**，故 `claude_pid` 的
> 去留裁定**尚未作出**（这正是 Ruling B 保留的那一步）。

**9c. 加载失败必须呈现给用户（账本第 46 条）**

`hooks_install::load()` 现在对"文件存在但读不出来"返回 `Err` —— 包括 **0 字节**或**带 BOM** 的 settings.json（旧代码会静默替换掉它们）。**`--install-hooks` / `--uninstall-hooks` 收到这个 `Err` 时必须把错误原样打印给用户**，不能当成意外崩溃或静默忽略。

**9d. 验收清单**

> **2026-09-14 界面改造后重写。** 用户实机看过之后的要求原话："UI 改成小游戏风格，白底黑边，
> 数码字体"、"背景换成白色，只需要显示工作状态即可，不需要显示具体的内容"。
> **每行现在只有「会名 + 状态 + 计时」**，所以原来那几条"能看到当前工具 / 上下文占比 / 步数"
> 的检查**已作废并删除**（那些现在**不该**出现，出现了就是 bug）；换成下面白底 / 黑边 /
> 数码字体 / 字号可读这几条。逐条决定见 `ui-restyle-report.md`。

- [ ] 跑 `--install-hooks`，确认 `~/.claude/settings.json` 的 `env` 块等原有内容没被动过，且 mtime/字节数变化符合预期
- [ ] 开两个终端各跑一个 Claude Code 会话，各发一句话
- [ ] 双击 `target/release/claude-hud.exe`，确认**同时出现两行**，且各自名字/状态正确
- [ ] 在其中 A 会话里触发一次权限请求，确认 A 那行变`等你确认`并排到最前
- [ ] **在 A 会话里按 Esc 打断**，确认 A 那行变为`被打断`而**不是**卡在`工作中`（这是 spec §7 第一个坑的验收点）
- [ ] 等 A 会话满 300 秒无活动，确认变`待命`但**仍在列表里**（不消失）
- [ ] 关掉 B 终端，确认 B 那行消失
- [ ] **白底**：面板填充是**纯白、不透明**（桌面**不**从面板里透出来）
- [ ] **黑边**：面板四周有**粗黑描边**（小游戏 HUD 的框感；1px 会像表格线，要求肉眼可见地粗）
- [ ] **黑字**：会名、计时、顶栏在纯白底上是**黑色**、对比足够（上一版"背景和文字颜色一个颜色"的抱怨必须消失）
- [ ] **状态词带状态色**，且在纯白底上**看得清**（尤其`等你确认`的琥珀色 —— 上一版为深灰底挑的 `#e3b341` 在白底上几乎看不见，已整体压暗）
- [ ] **数码字体**：**数字与 `:`**（`00:12` 计时、顶栏的 `3 个`、占比的 `68%`）呈现**数码管字形**；这是"数码字体"要求的落点。⚠️ **拉丁字母不走数码体** —— `web-lab`、`Claude` 应当是雅黑字形。本轮原先把"数字**与拉丁字母**"都写成要求，那是**第一版的行为，已被推翻**（用户第二轮裁定"数字用数码风格就好，其他的用微软雅黑"）；照旧条目验收会把正确实现判成**失败**
- [ ] **中文仍是汉字**：状态标签（`工作中`/`等你确认`/`被打断`…）、中文会话名、顶栏的`会话`/`个`、`▍` 色块都必须正常显示，**不能是缺字方框**（中文由系统中文字体兜底，见 ui-restyle-report.md 的两条 `has_glyphs` 断言）
- [ ] **字号可读**：会名 17pt（加粗）、状态与计时 15pt，隔一臂距离仍能读清
- [ ] **第一行只有三样**：会名 + 状态 + 计时。**不该**出现当前工具、步数、摘要。⚠️ **上下文占比与子代理是第二轮加回来的、应当出现**（在第二行）—— 本条原先把"上下文占比"列进"不该出现"，那是第一版的要求，已被推翻
- [ ] 确认 `config.json` 里已**不再有** `opacity`（字段已删；旧文件里留着它也不影响加载）
- [ ] 确认挂件窗口**没有控制台黑框**、置顶生效（**半透明已撤销，不再检查** —— 用户裁定不需要）
- [ ] 确认 `always_on_top: false` 时窗口**不再置顶**（该配置项此前被硬编码忽略，已修）
- [ ] **窗口能拖动**：在窗口的**任意非控件区域**按住左键即可移动窗口（窗口无装饰=没有标题栏，拖动靠 `ViewportCommand::StartDrag` 实现）
- [ ] **窗口拖不窄**：把窗口往窄里拖，应当**停在下限**（约 220px 宽）不再继续缩；此时状态词与计时仍**完整可见**，第二行也没压到黑框上。（此前窗口可拖到任意窄：<191px 第二行溢出黑框、<181px 被右缘裁、**<168px 连计时都被裁** —— 所以加了 `with_min_inner_size`。⚠️ **这三条线 2026-09-15 晚已作废**，见本文件末尾「第三轮」。）
- [ ] **界面无可感卡顿**：开窗口后状态立即出现，且随后每秒刷新时无卡顿、无迟滞（T12 已把轮询移出渲染线程，并修掉"每轮全量重扫 transcript"）
- [ ] **观感边界（无法单测，只能人看）**：白底黑边好不好看、数码体的实际观感、字号够不够大 —— 这三条**没有任何自动检查**，以用户看着实物说了算
- [ ] 跑 `--uninstall-hooks`，确认 `settings.json` 恢复到备份的内容（**不要**只对比"文件存在"）

- [ ] **Step 10: 提交**

```bash
git add -A
git commit -m "feat: egui 挂件渲染、main 参数分发、端到端集成测试（M1–M3 完成）"
```

---

## Task 12: transcript 接线（补齐上下文占比 / 当前工具 / 步数 / 打断）

**Files:**
- Create: `src/poller.rs`
- Modify: `src/main.rs`（加 `mod poller;`）
- Modify: `src/view.rs`（加 `build_rows_with_deltas`）
- Modify: `src/ui.rs`（轮询循环调用 poller）

**Interfaces:**
- Consumes: `transcript::{read_new, parse_delta, Delta}`、`state::SessionState`、`view::build_rows`
- Produces:
  - `poller::refresh(s: &mut SessionState, deltas: &mut HashMap<String, Delta>, now: i64) -> std::io::Result<()>`
    - 从 `s.transcript_offset` 读 transcript 新增部分，折叠进 `deltas[session_id]`，把 `ai_title` 写回 `s.display_name`，推进 `s.transcript_offset`
    - **绝不写状态文件**（Ruling #6：每个状态文件只能有一个写者 = hook 侧；挂件的内存副本可能陈旧，回写会覆盖 hook 刚写下的状态）
    - **不把 `delta.interrupted` 落到 `s.state`** —— 打断是渲染期由 `build_rows_with_deltas` 判定的（`interrupted` 优先于 `step_count == 0`），不污染持久化状态
    - > **更正：** 本行原写"把 `delta.interrupted` 落到 `s.state`…并保存状态文件"，两处都与 Ruling #6 及 `view.rs` 的实际设计冲突。Task 12 的实现者按代码与测试实现（不写文件），正确；此处为文档更正。
  - `view::build_rows_with_deltas(states: &[SessionState], deltas: &HashMap<String, Delta>, cfg: &Config, now: i64) -> Vec<Row>`
  - `view::build_rows(states, cfg, now)` 改为委托给上面这个（传空 map），Task 10 的测试因此**无需改动**即可继续通过

**关于 `Progress::Tasks`**：spec §4.2 实测确认你的会话里 `TodoWrite` 与 `Task` 各出现 **0 次**，`TaskCreated`/`TaskCompleted` 事件也未注册（§6 事件集固定为 8 个）。因此本任务实现的是 spec §2 选定的**智能回退分支**——恒为 `Progress::Steps(n)`。`Progress::Tasks` 变体保留定义但当前不可达，等将来真有数据源再接。

**关于 `Interrupted`**：这是 spec §7 第一个坑的正解。hook 侧永远感知不到 Esc，只有 transcript 侧的 `interruptedMessageId` 能看见它。

- [ ] **Step 1: 写失败测试**

```rust
// src/poller.rs
#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionState;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("claude-hud-poller-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn session(transcript: PathBuf) -> SessionState {
        SessionState {
            session_id: "p1".into(),
            cwd: "D:\\w".into(),
            transcript_path: Some(transcript.to_string_lossy().to_string()),
            display_name: None,
            claude_pid: None,
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
        fs::write(
            &t,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"a.rs"}}]}}"#,
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
        fs::write(
            &t,
            r#"{"type":"user","interruptedMessageId":"msg-1","message":{"content":""}}"#,
        )
        .unwrap();

        let mut s = session(t);
        let mut deltas = HashMap::new();
        refresh(&mut s, &mut deltas, 1000).unwrap();

        assert_eq!(s.state, "interrupted", "Esc 打断后状态必须变，不能卡在 working");
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
```

以及 `src/view.rs` 的接线测试：

```rust
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
            },
        );

        let rows = build_rows_with_deltas(&[st("a")], &deltas, &cfg, 100);
        let r = &rows[0];
        assert_eq!(r.detail.as_deref(), Some("Bash · cargo test"));
        assert_eq!(r.context_tokens, Some(363_026));
        assert!((r.context_pct.unwrap() - 0.363026).abs() < 1e-6);
        assert_eq!(r.progress, Progress::Steps(12));
    }

    #[test]
    fn session_without_delta_degrades_gracefully() {
        let cfg = Config::default();
        let rows = build_rows_with_deltas(&[st("a")], &HashMap::new(), &cfg, 100);
        assert!(rows[0].detail.is_none());
        assert!(rows[0].context_pct.is_none());
        assert_eq!(rows[0].progress, Progress::None);
    }

    #[test]
    fn build_rows_still_works_as_before() {
        // Task 10 的老签名必须保持可用
        let cfg = Config::default();
        assert_eq!(build_rows(&[st("a")], &cfg, 100).len(), 1);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test poller:: wire_tests`
Expected: 编译失败，`cannot find function 'refresh'` / `'build_rows_with_deltas'`

- [ ] **Step 3: 写最小实现（`src/poller.rs`）**

```rust
use crate::state::{self, SessionState};
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
pub fn refresh(
    s: &mut SessionState,
    deltas: &mut HashMap<String, Delta>,
    now: i64,
) -> io::Result<()> {
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

        // aiTitle 是比 cwd 末段好得多的会话名，拿到就覆盖
        if let Some(title) = &next.ai_title {
            s.display_name = Some(title.clone());
        }

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
```

- [ ] **Step 4: 写最小实现（`src/view.rs` 接线）**

把 Task 10 里的 `build_rows` 改为委托，并新增带 delta 的版本：

```rust
use crate::transcript::Delta;
use std::collections::HashMap;

pub fn build_rows(states: &[SessionState], cfg: &Config, now: i64) -> Vec<Row> {
    build_rows_with_deltas(states, &HashMap::new(), cfg, now)
}

pub fn build_rows_with_deltas(
    states: &[SessionState],
    deltas: &HashMap<String, Delta>,
    cfg: &Config,
    now: i64,
) -> Vec<Row> {
    let mut keyed: Vec<(Row, i64)> = states
        .iter()
        .map(|s| {
            let delta = deltas.get(&s.session_id);

            let raw = State::from_str(&s.state);
            let state = decay(raw, s.state_since, cfg, now);
            // spec §7 第一个坑：Esc 打断不触发任何 hook，只有 transcript 侧的
            // interruptedMessageId 能看见它。它是权威的，优先于 hook 推出来的状态。
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

            let detail = delta.and_then(|d| match (&d.last_tool, &d.last_tool_detail) {
                (Some(t), Some(x)) => Some(format!("{} · {}", t, x)),
                (Some(t), None) => Some(t.clone()),
                _ => None,
            });

            let context_tokens = delta.and_then(|d| d.context_tokens);
            let context_pct = context_tokens
                .map(|t| (t as f64 / cfg.context_limit.max(1) as f64) as f32);

            // spec §2 的"智能回退"：有任务清单时本应显示 n/N，但 spec §4.2 实测
            // TodoWrite / Task 各 0 次，且 §6 的事件集不含 TaskCreated/TaskCompleted，
            // 故当前恒走 Steps 分支。Progress::Tasks 保留定义，等真有数据源再接。
            let progress = Progress::Steps(delta.map(|d| d.step_count).unwrap_or(0));

            let subtitle = match state {
                State::Done | State::Error | State::Interrupted => s
                    .last_assistant_message
                    .clone()
                    .or_else(|| s.notification_message.clone()),
                State::Waiting => s.notification_message.clone(),
                _ => None,
            };

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
                },
                // 次级排序键。`Row` 的接口里没有它，故不能进 `Row`；
                // 但 spec §9 要求「同级取最近有**事件**的那个」，必须带上。
                s.last_event_at,
            )
        })
        .collect();

    // ⚠️ 次级键是 `last_event_at` 降序，**不是** `elapsed_secs` 降序 —— 见 Task 10 同处的说明。
    keyed.sort_by(|(a, a_at), (b, b_at)| {
        priority(a.state)
            .cmp(&priority(b.state))
            .then_with(|| b_at.cmp(a_at))
    });

    keyed.into_iter().map(|(row, _)| row).collect()
}
```

- [ ] **Step 5: 接入 `ui.rs` 轮询循环**

```rust
struct App {
    cfg: config::Config,
    rows: Vec<view::Row>,
    deltas: std::collections::HashMap<String, transcript::Delta>,
    last_poll: Instant,
}

impl App {
    fn poll(&mut self) {
        let dir = paths::real_sessions_dir();
        let mut states = state::list_all(&dir);

        // 先刷新每个会话的 transcript 派生信息，再据此生成行。
        //
        // ⚠️ 控制器裁定（Ruling #6，pre-flight）：**挂件绝不写状态文件。**
        // 每个状态文件只能有一个写者（hook 侧）。挂件手里的内存副本可能是旧的
        // （state=working），回写会覆盖 hook 刚写下的 state=waiting —— 原子写只
        // 保证不读到半截文件，不保证不丢更新。故 offset 与 Delta 一样只活在挂件
        // 进程内存里；重启后重扫一次 transcript 作一次性开销。
        let now = chrono::Local::now().timestamp();
        for s in states.iter_mut() {
            let _ = poller::refresh(s, &mut self.deltas, now);
        }

        self.rows = view::build_rows_with_deltas(&states, &self.deltas, &self.cfg, now);
        self.last_poll = Instant::now();
    }
}
```

注意：`deltas` 是**挂件进程内存里的累积状态**（不是每轮重算），这样 `step_count` 才能跨轮次累加。挂件重启后步数会从 0 重新开始——这是可接受的，因为重启后本来也要从当前 offset 续读。**但若要精确，应在 `SessionState` 里持久化 `Delta`**；当前实现选择不持久化，以保持状态文件 schema 与 spec §8 一致。

- [ ] **Step 6: 运行测试确认通过**

Run: `cargo test`
Expected: 全绿（Task 1–12 全部单测 + 2 个集成测试）

- [ ] **Step 7: 提交**

```bash
git add -A
git commit -m "feat: transcript 接线，补齐上下文占比/当前工具/步数/打断检测"
```

- [ ] **Step 8: 删除所有临时的死代码抑制属性（履行 Ruling #7 的到期义务）**

Task 2–9 期间，每个新模块都按 Ruling #7 在 `src/main.rs` 的 `mod` 声明上加过 `#[allow(dead_code)]`，注释里写着"可达消费者在 Task 11 出现、届时应删除"。**现在才是删除的时机** —— 不是 Task 11。

**为什么不是 Task 11：** Task 11 只接上了 `ui` 与 `main`，`transcript::Delta` 的字段（`context_tokens` / `ai_title` / `last_tool` / `last_tool_detail` / `step_count` / `interrupted`）在本任务（Step 4–5）之前**没有任何读者**。Task 9 的评审核查过：`grep parse_delta|Delta|summarize_tool` 在 `transcript.rs` 之外零匹配。**在 Task 11 删除会让 `cargo build` 冒出 "field is never read"** —— 把降噪换成真噪音。本任务的 Step 4–5 之后所有条目才全部可达。

**做法：** 删掉 `src/main.rs` 上全部 7 个 `#[allow(dead_code)]` 及其上方注释（`paths` / `config` / `hook` / `model` / `state` / `hooks_install` / `transcript`），然后跑：

```bash
RUSTUP_HOME='D:\rust\rustup' CARGO_HOME='D:\rust\cargo' "D:/rust/cargo/bin/cargo.exe" build 2>&1 | grep -c "^warning"
```

**期望 `0`，但 0 不是硬指标 —— 真实的判据是"没有被人为压制的死代码"。**

Task 12 执行后实测为 **7 条**，逐条查明如下，**全部属合法闲置**，故**接受**、且**不加回任何属性**：

| 类别 | 条目 |
|---|---|
| 测试专用辅助 | `config::save_to` / `config::tmp_for`（仅自测调用） |
| 保留的 schema 字段 | `HookPayload` 的 3 个字段（schema 完整性） |
| brief 明令保留 | `Progress::Tasks`（等真有数据源）、`build_rows`（老签名，现为一行委托） |
| 后续里程碑 | `aggregate`（M4 折叠胶囊）、`Row` 的 `session_id`/`context_tokens`/`context_limit` |

**其中无一条来自 `transcript.rs`** —— 即"接线后 `Delta` 的六个字段不再报警"这一论断**成立**。

**执行者的义务是：** 剩余警告**逐条可解释**，不得有"不知道谁在用"的警告；**遇到无法解释的必须查明，而不是用属性盖住**。删属性正是这条义务要消灭的东西。

- [ ] **Step 9: 提交清理**

```bash
git add src/main.rs
git commit -m "chore: 删除临时的 dead_code 抑制属性（Ruling #7 到期履行）"
```

---

## Self-Review

写完计划后对着 spec 逐节回查了一遍。

**1. Spec 覆盖度**

| spec 章节 | 落在哪个任务 |
|---|---|
| §5 架构：单 exe 双模式 | Task 6（`--hook`）、Task 11（`main` 分发） |
| §5 零外部依赖 | Task 1 依赖清单（无网络/DB crate），全局约束 |
| §5 目录布局 | Task 1（paths）、Task 11（src/*.rs 全部落地） |
| §6 八个 hook 事件 | Task 7 的 `EVENTS` 常量 |
| §6 不做 per-tool 埋点 | 全局约束 + Task 7 只注册 8 个事件 |
| §7 状态机全部分支 | Task 4（11 个单测逐条覆盖） |
| §7 坑 1：Esc 打断 | Task 9（检测）+ Task 12（落到 `state`）+ Task 11 Step 9 验收项 |
| §7 坑 2：僵尸会话 | Task 7 Step 5–6（真机验证父进程链后二选一） |
| §7 坑 3：SubagentStop 不清 waiting | 我们不注册该事件，从源头规避 |
| §8 状态文件 schema | Task 5 的 `SessionState` |
| §8 config schema | Task 2 的 `Config`（含 `idle_after_sec`） |
| §9 会话名取 aiTitle | Task 9（解析 `ai_title`）+ Task 10（回退链）+ Task 12（写回 `display_name`） |
| §9 上下文占比与配色阈值 | Task 12（`poller` 接线 + `context_pct`）+ Task 11（着色） |
| §9 进度行智能回退 | Task 10 的 `Progress` 枚举 + Task 12 接线（恒走 `Steps` 分支） |
| §9 当前动作行 | Task 9（`summarize_tool`）+ Task 12（拼成 `"Bash · cargo test"`） |
| §9 排序与聚合优先级 | Task 10 的 `priority()` + `aggregate()` |
| §10 M1/M2/M3 | Task 1–11 全覆盖 |
| §10 M4 外观打磨 | **不在本计划**，见下 |
| §10 M5 打包自启 | **不在本计划**，见下 |
| §11 待验证事项 5（projects 目录名有损） | 全局约束：cwd 只从 payload 取 |
| §12 明确不做 | 全局约束逐条对应 |

**自审中补上的一整个任务（Task 12）。** 初稿把 `context_pct` / `detail` / `progress` 三项留成空值，并把接线推给一个"以后再说"的任务。这是自审规则明令禁止的——"若发现某条 spec 要求没有对应任务，就补上那个任务"。已验证属实后补了 Task 12：`poller::refresh` 负责 IO 编排（读 transcript 增量 → 折叠 delta → 回写 `display_name` / `state` / `offset`），`view::build_rows_with_deltas` 负责纯逻辑映射。拆分点选在"IO 与纯函数之间"，两边各自可测。

**`Progress::Tasks` 变体当前不可达，这是如实反映现状而非缺口。** spec §4.2 实测你的会话里 `TodoWrite` 与 `Task` 各 0 次，§6 也把事件集固定为 8 个（不含 `TaskCreated`/`TaskCompleted`）。所以 spec §2 选定的"智能回退"在本计划里的正确实现就是**恒走 `Steps` 分支**。变体保留定义，等真有数据源时再接——实施者**不得**为此编造数据来源。

**2. 占位符扫描**：无 TBD / TODO / "稍后补充" / "类似 Task N"。所有代码步骤都带可直接粘贴的完整代码。

**3. 类型一致性**

- `State` 枚举在 Task 4 定义（`Idle/Working/Waiting/Compacting/Done/Error/Interrupted`）。字符串→枚举的反向转换 `State::from_str` 定义在 `model.rs`（Task 6 加入），供 Task 6 与 Task 10 共用。
- **修正记录：** 本段初稿曾主张"Task 6 与 Task 10 各自实现 `state_from_str` 是**有意的**，因为合并会让 `model` 与 `state` 循环依赖"。**该理由不成立，已撤回** —— `model.rs` 与 `state.rs` 互不依赖，两者都只依赖 `model.rs`，把转换放进 `model.rs` 不构成环，而逐字重复的逻辑块会被评审规则判为缺陷。pre-flight 扫描据此裁定集中到 `model.rs`；T6 已落地（含往返测试），T10 的计划段已同步改调它。
- `SessionState` 字段名在 Task 5 定义，Task 6/10 使用一致（`state_since`、`last_event_at`、`transcript_offset`）。
- `Config.context_limit` 类型 `u64`，Task 10 里 `cfg.context_limit.max(1) as f64` 做了除零保护。
- `transcript::Delta` 在 Task 9 定义，Task 11 的 `ui.rs` **尚未引用**——接线任务会用到。
- `paths::real_sessions_dir()` 在 Task 11 Step 3 引入，集成测试与 `ui.rs` 都用它。

**4. 自审中发现并已修掉的 bug**：Task 8 原实现把"半行"也计入 offset，会导致 transcript 记录永久丢失。已补 `partial_line_is_not_consumed` 测试并把实现改为只消费到最后一个换行。

---

## 本计划不含的部分（留给后续计划）

**M4 外观打磨 / M5 打包自启不在本计划内，这是有意的。** 理由：呼吸动画的节奏、圆角半径、胶囊折叠后的大小与位置——这些**看着文字拍板没有意义**，必须在东西已经跑起来、能盯着看的时候定。提前写进计划只会产生一份到时候必然被推翻的伪精确规格。

（本条原先还列了"`opacity` 的合适取值"作为一个待定项 —— `opacity` 已随"不做半透明"删除，故移除。）

M1–M3 完成后会得到：一个能跑、能显示多会话实时状态的 exe。那时再为 M4/M5 单独走一轮 brainstorming → writing-plans。

> **2026-09-14 更正：** 实际上 **M4 的一部分外观决策已经在 M1–M3 期间做了** —— 用户实机拿着能跑的东西直接拍板（白底黑边、数码字体、字号、只留哪些信息）。这正是上面"必须能盯着看才能定"那句话的正面印证，所以并轨合理、不算越界。**M4 剩下的部分**（圆角 / 位置记忆 / 右键菜单 / 呼吸动画 / 折叠胶囊）与 **M5**（打包发布 / 开机自启）仍未做。

---

## 搁置：token 视图（2026-09-14 用户决定先搁置）

> ⚠️ **2026-09-16：本段已「解搁置」** —— 用户提了新需求（各项目 / 全机 token + 子代理折线），
> 计划另起：**`2026-09-16-token-and-subagent-link.md`**（含六条实测事实与三条硬要求）。
> 下面这段**仍然是口径的权威来源**（四桶平铺、按 uuid 去重、按行内 timestamp 过滤、启动全扫），
> 新的那份计划在它之上补了三条：`mtime` 预筛、跨文件全局去重、**按行取 `cwd` 归属项目**。

**要做什么**：在挂件里加一个 token 用量视图 —— **每个会话后面接上它自己的 token 用量，底部一行「今日总计」**。

**用户已定完整设计（三条裁定）：**

| 项 | 决定 | 备注 |
|---|---|---|
| 口径 | **四个桶平铺相加**（`input` + `output` + `cache_read` + `cache_creation`） | 用户明确"不要钱，只要 token 数"，故不做加权、不折算金额 |
| 拆解 | **只要一个总数**，不列四项 | 已知代价：平铺相加 ≈ **99.8% 来自 `cache_read`**，所以这个数本质上在量"同一段上下文被重复读了多少遍"。**用户知情并接受** |
| 范围 | **含今天已结束的会话** | 否则关掉终端用量就从总计里消失，那个数会跳 |
| 摆放 | **直接接在每行后面** + 底部一行今日总计 | |
| 行内数字口径 | **该会话自身的整个 transcript 累计**（**不是**今日部分） | 依据：用户原话只给"总计"加了时间限定 |

**于是必然出现：今日总计 ≥ 行内之和。** 因为总计还包含今天开过、现在已关掉的会话（它们不在列表里）。**这是设计，不是 bug** —— UI 上要把总计行标清楚。

### 三条硬要求（缺一条数字就是错的）

1. **必须按 `uuid` 去重。** transcript 里有**约 5% 重复行**（1044 个重复 uuid / 2088 行）。claude-hud 现在"取最新一条覆盖"所以侥幸免疫，**一旦累加不去重就虚高 5%**。
2. **按行内 `timestamp` 过滤"今天"**，不能只看文件 mtime —— 一个昨天开、今天还在跑的会话，文件是今天的，但里面混着昨天的行。
3. **启动全扫 + 之后增量维护**，且**必须在后台线程**。选了"含已结束"就必须有一次全量扫描；而轮询早已移出渲染线程，别再搬回去。

### 顺带的收益（用户知悉）

这条功能会**实测掉 `token-desk` 挂着的两个未知数**：①今日 token 能否算准 ②全扫 371.8MB 耗时。做完把这两个数交给 token-desk，它开工时直接可用。

> **这也是为什么"不合并代码库"的判断依旧成立**：真正重复的只有"解析同一批 jsonl"这一件事。**抽共享解析层**是合理的；**把两个产品合成一个**是另一回事 —— 两者 **GUI 层不相容**（claude-hud = egui/eframe，token-desk = win32 + Direct2D）、**生命周期相反**（hook 驱动的事件流轻进程 vs 需启动全量索引）。

---

## 后续（外观待办 —— 用户 2026-09-14 提出）

> **✅ 2026-09-15：四项全部处理完毕**，用户实机逐条看过并追加了第二轮裁定。本节从"待办"
> 改成"**记录结论**"；仍在的开口见末尾的「仍未做」。

| # | 当时的待办 | 结论（2026-09-15） |
|---|---|---|
| ① | 上下文与占比那一段的**字体、字号** | **做完了**：字号 = 状态词的一半（15 → **7.5pt**）；`%` 归中文族（用户："不用纠结无所谓"） |
| ② | **布局优化** | **做完（分两轮）**：第二轮做最小窗口宽度 220、默认 320×360、会话间距**翻倍**（29 → 58pt，实测）；**第三轮（2026-09-15 晚）做对齐基准与四段纵向间距**，见下节 |
| ③ | **显示「当前具体任务」** | **做了又删了**。先按"两个都要"做了（第二行画 `Bash · xxx`），用户当天看过之后裁定"**去掉当前工作具体内容，不需要显示这个信息**"。会话**回到两行** |
| ④ | **大标题优化** | **做完了**：顶栏统一**不走数码体**（新增一条不含子集的字体族） |

**用户 2026-09-15 追加的裁定**（四条，都已落地）：

- 会话标题也**不要数码体** —— 连同"截断额度必须按同一条族算"一起改（数码数字比雅黑宽 39%，
  拿数码宽度算额度会过早截断）。
- **标题固定不动**：照 VS Code 的链取，但**跳过 `lastPrompt`**、改用 `firstPrompt`
  （前者会让你刚打的字变成标题，每隔几秒变一次）。详见 spec §9。
- **窗口要能缩放**：无边框窗口没有系统热区，自己划了 6pt 边缘带（`BeginResize`）。
- **后台子代理在跑时，会话必须显示"工作中"** —— 原来的判据把"启动回执"当成"完成"，
  于是主代理 `Stop` 后显示"完成/待命"，与事实相反。详见 spec §9。

### 第三轮（2026-09-15 晚）：右缘基准 + 四段纵向间距

**用户裁定**："状态词+计时右对齐到右缘"。做下去发现纵向间距也得一起重定 —— 两件事同一个根因。

**根因**：间距是 `add_space` 与三处**隐式份量**叠加出来的（`item_spacing.y` **= 3**、`ui.horizontal`
的块高 **+5**、`separator()` 自带的 **6pt** 占位带）—— 常量名与界面上的数对不上。
能复现的版本：**`EXTRA_ROW_GAP = 33` 在改前量到 58，清掉隐式量之后同一个 33 只量到 49**。
（旧文"写 33 得到 29、写 11 得到 19"**已作废**：29 是三行布局时代的数；19 = 11 + 第二行字高 8，
那 8 是**字高**不是间距。2026-09-16 两版探针复量确认。）改法是把三处全部消掉（`item_spacing.y` 清零、行高用 `row_line`
显式钉死、分隔线 `spacing(0.0)`）：**凡是"间距"就不允许有隐式份量。**

**右缘基准**的落点由 `paint_right` 自己算坐标直接 `painter.galley` —— 不能用
`with_layout(right_to_left)`：实测在本文件"上一段是截断过的会名"的场合下，它的**分配**对、
**绘制**却从 `max_rect.right()` **往右**画，220pt 窗宽下 `00:41` 被画成 `00:4` + 半个 `1`。
自己算落点还顺带让"右缘贴齐"**可以被单测断言**（走 `with_layout` 时 `Shape::Text.pos` 报的坐标是错的）。

| 项 | 裁定值 | 改前 |
|---|---|---|
| 顶栏 → 第一条会话 | **16pt** | 6pt（比会话内部两行之间的 8pt 还紧，层级是倒的） |
| 第一行 → 第二行 | **14pt** | 8pt |
| 第二行 → 分隔线 | **8pt** | 11pt（第二行离下面的线比离它注释的那行还远 = 掉队注脚） |
| 第二行 → 下一会话会名 | **58pt** | 58pt（旧裁定不许被这轮悄悄推翻，故按净增量补了 `EXTRA_ROW_GAP`）。⚠️ 这一格是 **galley 原点差**，其余三格是**视觉空隙** —— 同处视觉空隙为 **50**，别混着比 |

**一条关于"怎么测"的坑**：第二行 origin 间距改前是 **30pt（改后 36）**、看着很健康，但它在屏幕上离下面的线
仍然更近 —— 因为**第二行只有 8pt 高**。判断分组感要量**视觉空隙**（前一段的 ink 下缘 →
后一段的 ink 上缘），不是 galley 原点的差。

**最小窗口宽度的依据已改写**：`< 191px 第二行压过黑框 / < 181px 被右缘裁 / < 168px 连计时
都被裁` 这三条**全部作废**（成立于 `CONTEXT_SIZE` 还是 15pt、且计时与第二行都还是顺序左排的
年代）。右对齐之后计时**不参与布局**，实测 140pt 宽下右缘仍精确落在内容右界上。
现在真正的退化只剩会名：**170pt 整个消失 / 190pt 只剩 `…` / 200pt `示…` / 220pt `示例…`**。

### 仍未做（原四项之外的）

- **布局细调**：对齐基准与四段纵向间距已在第三轮定完。**还没碰的**：色条/会名的横向起点与
  缩进、各段宽度分配（顶栏与内容是否共用同一条右缘）、顶栏自身的样式、默认窗口尺寸是否随
  行高变化而调。
  > ⚠️ 最后一项**已实测到底**（2026-09-16）：默认 320×360 在 **4 个会话时余 1pt**
  > （第 4 个的收尾分隔线已在可视区外，看着像被切），**第 5 个起整行消失且没有任何提示**
  > （无滚动条、无"还有 N 个"）。每会话约 94pt，360 减去顶栏只剩约 305pt。
  >
  > **用户 2026-09-16 裁定：暂不处理**（记在此处备查，别再当"待办"追问）。
  > 要动的时候四条路：①加高默认 + 末尾"+N 个未显示"（最小改动）；②高度随会话数自适应；
  > ③折叠胶囊（`view::aggregate` 已实现，就差调用方）；④加滚动区。
- **M4 其余部分**：圆角 / 位置记忆（`config.window_pos` 现在是**死配置**，窗口能拖但位置不
  持久化；`config::save_to` 是现成件）/ 右键菜单 / 呼吸动画 / 折叠胶囊（`view::aggregate`
  已实现但无调用方，就是为它准备的）。
- **M5**：打包发布 / 开机自启。

> **改外观前的一条硬提醒**：`src/ui.rs` 已 **近 3000 行**，且**字体分工（哪个字符走哪条族）
> 散在多处注释与辅助函数里**。改前先读它顶部的字体分工说明、以及 spec §9 的字体分工表，
> 否则很容易把数码体/雅黑的分工弄乱 —— 这个坑第一轮踩过一次（完整 DSEG 把拉丁字母也抢走了），
> 第二轮又差点踩（会名改族时**忘了同步截断额度的度量族**，那会让带数字的名字被过早截断）。

**已知需要在 M4 之前处理的欠账**（由实施者交接）：
1. **Task 7 Step 5–6 的结论要落实到代码**：真机验证 hook 进程的父进程链之后，决定保留 `claude_pid` 走真实存活检测，还是改用 `zombie_after_sec` 超时回收。这是本计划里唯一一处"先测后定"的地方。
   > ✅ **已结（2026-09-15）。** 结论：hook 的 hop 1 **就是 `claude.exe`**（exec 形式注册、
   > 不过 shell），宿主 pid 对同一个 `session_id` **恒定** ⇒ **保留 `claude_pid`，走真实存活
   > 检测**，不引入超时回收旋钮。实现见 `src/procinfo.rs` / `hook.rs` / `ui.rs`，
   > 完整取证与裁定记在 `docs/工作日志.md` 的「2026-09-15 · 9b 僵尸会话」一节，
   > 规格同步在 `specs/…design.md` §7 坑 2。
   >
   > 一段值得留痕的过程：**探针的第一版是覆盖写、不带 `session_id`**，于是头两份样本的宿主
   > pid 不同、看起来像"宿主会变"；补上 `session_id` 才看出那两份本来就来自**不同会话**
   > （本机同时开着 3 个 `claude.exe`）。另外，原方案的硬伤在于 **hook 进程的环境变量无法由
   > 外部注入、必须重启 Claude Code 才测得到** —— 这才是它一直悬着的原因；本轮加了"哨兵
   > 文件"这条运行时启用的路才把测量做掉。
2. **`deltas` 目前只活在挂件进程内存里**：挂件重启后步数从 0 重算。精确的做法是把 `Delta` 持久化进 `SessionState`——但那会改动 spec §8 的状态文件 schema，**需要先与用户确认**，不在本计划范围内擅自改契约。


