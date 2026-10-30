# claude-hud

Windows 桌面常驻小挂件：一眼看清**多个并发 Claude Code 会话**的处境 —— 哪个在跑、哪个等你确认、
哪个跑完了、哪个被打断。数据只读本机 `~/.claude/` 下的文件。

## 安装

1. 从 [Releases](https://github.com/BoloJoe-stack/claude-HUD/releases) 下载 `claude-hud.exe`，
   放到一个**以后不会移动**的目录
   - 就一个 exe，**不需要装 Rust、不需要 VC++ 运行库、不需要 .NET / Node**
     （C 运行库已静态链接进 exe，干净 Windows 双击即跑）
2. `claude-hud.exe --install-hooks` —— 把 8 个事件写进全局 `~/.claude/settings.json`
   （只新增 `hooks` 键，你原有的 `env` 等配置一字不动；重复执行不会翻倍）
3. 双击 `claude-hud.exe` 起挂件

卸载 `claude-hud.exe --uninstall-hooks` · 自检 `claude-hud.exe --doctor`

⚠️ `--install-hooks` 会改写**全局** settings，影响所有 Claude Code 会话，执行前建议先备份。
⚠️ hook 记的是 exe 的**绝对路径** —— 挪动 exe 之后要重新 `--install-hooks`。
⚠️ 本项目**未做代码签名**。从浏览器下载的 exe 首次运行时，Windows SmartScreen 会提示
「Windows 已保护你的电脑」，点「更多信息 → 仍要运行」即可；可用 Release 正文里的 SHA256
核对文件完整性。

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

**症状**：模型的真实上下文上限不是 1M 时（例如 200K），不改这个值会让「上下文 N%」只有真实
占比的 1/5 左右 —— 数字本身合理，不会报任何错。启用后请核对一次：若占比增长明显偏慢，
按实际的上限修改 `context_limit` 并重启挂件。

## 系统要求

- **Windows 10 / 11 · x64**。Windows 7 与 32 位系统**不能运行**（exe 静态导入的系统库
  自 Windows 8 起才有）；Windows 8 / 8.1 与 Windows 11 ARM64 未测试。
- 不需要管理员权限；配置写在 `%APPDATA%\claude-hud\`，hook 写在用户目录的 `.claude\settings.json`。
- 界面中文使用**系统自带**的中文字体（Windows 10/11 常规安装均有）。缺少中文字体的环境
  （精简镜像、Server Core 等）启动时会弹框提示，但界面文字会显示为方框。
- 只读取本机 Windows 用户目录下的 `~/.claude/`。Claude Code 跑在 **WSL、远程 SSH 或容器**里时，
  转录与 hook 都在另一套文件系统里，挂件读不到这些会话。
- 目前没有开机自启；重复双击会打开两个相同的挂件。

## 从源码构建

需要 Rust（`stable-x86_64-pc-windows-msvc`）：

```bash
cargo build --release      # → target/release/claude-hud.exe
```

仅 Windows 10/11 x64；exe 无运行时依赖（不需要 .NET / WebView / Node）。

⚠️ 仓库里的 `.cargo/config.toml` 给 MSVC 目标加了 `+crt-static` —— **别删**。
没有它，Rust 默认动态链接 `VCRUNTIME140.dll`，在没装 VC++ 运行库的机器上会报
"计算机中丢失 VCRUNTIME140.dll"。改完构建后用下式自检，输出里**不得**出现
`VCRUNTIME140.dll`：

```bash
objdump -p target/release/claude-hud.exe | grep "DLL Name"
```

## 许可

- 本项目 **MIT**，见 `LICENSE`
- 随二进制打包的 `assets/claude-hud-digits.ttf` 是 **DSEG14 的数字子集**（© keshikan，
  SIL OFL 1.1；按 OFL 条件 3 已改名），完整声明见 `assets/` 下两个文件
- 中文与拉丁字母使用系统自带的微软雅黑，不打包

## 更多

逐条的决策、实测数字与踩过的坑见 [`docs/工作日志.md`](docs/工作日志.md)。
