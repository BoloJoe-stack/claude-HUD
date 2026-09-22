# claude-hud

Windows 桌面常驻小挂件：一眼看清**多个并发 Claude Code 会话**的处境 —— 哪个在跑、哪个等你确认、
哪个跑完了、哪个被打断。数据只读本机 `~/.claude/` 下的文件。

## 安装

1. 从 [Releases](https://github.com/BoloJoe-stack/claude-hud/releases) 下载 `claude-hud.exe`，
   放到一个**以后不会移动**的目录
2. `claude-hud.exe --install-hooks` —— 把 8 个事件写进全局 `~/.claude/settings.json`
   （只新增 `hooks` 键，你原有的 `env` 等配置一字不动；重复执行不会翻倍）
3. 双击 `claude-hud.exe` 起挂件

卸载 `claude-hud.exe --uninstall-hooks` · 自检 `claude-hud.exe --doctor`

⚠️ `--install-hooks` 会改写**全局** settings，影响所有 Claude Code 会话，执行前建议先备份。
⚠️ hook 记的是 exe 的**绝对路径** —— 挪动 exe 之后要重新 `--install-hooks`。

## 配置

`%APPDATA%\claude-hud\config.json`（这个文件不存在也能正常工作，用默认值）。

| 字段 | 默认 | 说明 |
|---|---|---|
| `context_limit` | `1000000` | 上下文上限，用于算占比。**必须手配** |
| `warn_threshold` | `50` | 占比达到此值转琥珀色 |
| `danger_threshold` | `75` | 占比达到此值转红色 |
| `poll_interval_ms` | `1000` | 轮询周期 |
| `idle_after_sec` | `300` | 完成多久后显示为「待命」 |
| `always_on_top` | `true` | 窗口置顶 |
| `window_pos` | `[100, 100]` | 窗口位置，拖动后自动记住 |

**上下文上限为什么必须手配**：会话文件里只记录 model ID，没有任何窗口大小字段，上限无法从
文件推断；按 model ID 猜在某些模型上会**静默算错数倍**，而数字看起来完全合理。

## 从源码构建

需要 Rust（`stable-x86_64-pc-windows-msvc`）：

```bash
cargo build --release      # → target/release/claude-hud.exe
```

仅 Windows；exe 无运行时依赖（不需要 .NET / WebView / Node）。

## 许可

- 本项目 **MIT**，见 `LICENSE`
- 随二进制打包的 `assets/claude-hud-digits.ttf` 是 **DSEG14 的数字子集**（© keshikan，
  SIL OFL 1.1；按 OFL 条件 3 已改名），完整声明见 `assets/` 下两个文件
- 中文与拉丁字母使用系统自带的微软雅黑，不打包

## 更多

逐条的决策、实测数字与踩过的坑见 [`docs/工作日志.md`](docs/工作日志.md)。
