# claude-hud

Windows 桌面常驻小挂件：一眼看清**多个并发 Claude Code 会话**的处境 —— 哪个在跑、哪个等你确认、
哪个跑完了、哪个被打断。数据只读本机文件（hook 写入的 `%APPDATA%\claude-hud\sessions\`
与 `~/.claude/` 下的转录），零网络。

## 安装

1. 从 [Releases](https://github.com/BoloJoe-stack/claude-HUD/releases) 下载 `claude-hud.exe`，
   放到一个**以后不会移动**的目录
   - 就一个 exe，**不需要装 Rust、不需要 VC++ 运行库、不需要 .NET / Node**
     （C 运行库已静态链接进 exe，干净 Windows 双击即跑）
   - **不要直接在 zip 里双击运行**：Windows 会把 exe 解到临时目录再执行，hook 记下的
     就是那个临时路径，重启后即失效
2. 双击 `claude-hud.exe`。**第一次启动会弹框问要不要注册 8 个 hook**，点「是」即可
   （写进全局 `~/.claude/settings.json`，只新增 `hooks` 键，你原有的 `env` 等配置一字不动；
   以后不再问）。也可以手动运行 `claude-hud.exe --install-hooks`
3. **重启已经开着的 Claude Code 会话**。hook 的配置只在会话启动时加载，装 hook 之前
   就开着的会话不会补发事件，挂件里永远看不到它们（之后新开的会话正常）

确认生效：顶栏出现 `● claude 会话 · N 个`，N 与当前实际会话数一致。若显示 `0 个` /
「没有活跃会话」，按下面「hook 没生效怎么查」逐条核对。

卸载 `claude-hud.exe --uninstall-hooks` · 自检 `claude-hud.exe --doctor` · 版本 `claude-hud.exe --version`

⚠️ `--install-hooks` 会改写**全局** settings，影响所有 Claude Code 会话，执行前建议先备份。
⚠️ hook 记的是 exe 的**绝对路径** —— 挪动 exe 之后要重新 `--install-hooks`。
⚠️ 本项目**未做代码签名**。从浏览器下载的 exe 首次运行时，Windows SmartScreen 会提示
「Windows 已保护你的电脑」，点「更多信息 → 仍要运行」即可；可用 Release 正文里的 SHA256
核对文件完整性。

## 会话数是哪来的

挂件**不扫描正在运行的进程**，只读 8 个 hook 写下的状态文件：
`%APPDATA%\claude-hud\sessions\<session_id>.json`。所以：

- 一行 = 一个**会话**，不是一个窗口；同一窗口里的两个终端算两个会话
- 被别的会话拉起的 `claude -p` 子会话不单独列（它干的活算在父会话那行）
- hook 没生效 → 一个状态文件都没有 → 顶栏 `0 个`
- 会话在装 hook **之前**就开着 → 它没有状态文件 → 不出现，重开一次即可

**自动缩放**：会话条数变化时，窗口高度贴到刚好装下（宽度与左上角不动）；正在手动拖动 /
缩放时不干预；首轮扫描完成前不动；手动拉出来的高度不会被每帧拽回去。

## hook 没生效怎么查

按顺序核，基本三步内能定位：

1. `claude-hud.exe --doctor` —— 看两项：
   - 「hook 未注册（读不到 settings.json）」「hook 缺 N 个事件」
     「hook 指向的 exe 不存在 —— 8 个事件都会静默失效」
   - 「状态目录不存在 —— hook 从来没成功写过东西」
     「最近 24 小时没有任何 hook 写过状态文件」
2. 打开 `%USERPROFILE%\.claude\settings.json` 的 `hooks`：8 个事件（`SessionStart` /
   `UserPromptSubmit` / `Notification` / `PreCompact` / `PostCompact` / `Stop` /
   `StopFailure` / `SessionEnd`）的 `command` 是否就是**当前这个 exe 的真实路径**。
   挪过 exe、换过版本目录，就在这里断链 —— 重跑一次 `--install-hooks`。
3. **重启 Claude Code 会话**后再看（安装第 3 步之后没有重启，老会话不会出现）。
4. 看 `%APPDATA%\claude-hud\sessions\` 里有没有 `.json` 在增加；不增加 = hook 没跑。
5. 设了 `CLAUDE_CONFIG_DIR` 的机器：当前版本只认 `%USERPROFILE%\.claude`，**不支持**
   自定义配置目录，会表现为 0 个会话。
6. Claude Code 跑在 WSL / 远程 SSH / 容器 / 另一个 Windows 用户下：hook 与转录都在
   另一套文件系统里，挂件读不到（见「系统要求」）。

## 配置

`%APPDATA%\claude-hud\config.json`（这个文件不存在也能正常工作，用默认值）。

| 字段 | 默认 | 说明 |
|---|---|---|
| `context_limit` | **未设置** | 上下文上限。首次运行会问一次；未设置时只显示 token 数、不显示百分比 |
| `warn_threshold` | `50` | 占比达到此值转琥珀色 |
| `danger_threshold` | `75` | 占比达到此值转红色 |
| `poll_interval_ms` | `1000` | 轮询周期 |
| `idle_after_sec` | `300` | 完成多久后显示为「待命」 |
| `always_on_top` | `true` | 窗口置顶 |
| `window_pos` | `[100, 100]` | 窗口位置，拖动后自动记住 |

**为什么首次运行会问这一次**：会话文件里只记录 model ID，没有任何窗口大小字段，上限无法从
文件推断；按 model ID 猜在某些模型上会**静默算错数倍**，而数字看起来完全合理。所以程序
不猜，只问一次：**是 = 1,000,000 / 否 = 200,000 / 取消 = 先不设置**。选错了随时改
`config.json` 后重启挂件即可。

**没设置时**：第二行显示 `上下文 --` 和 token 数，**不会**按某个默认值算出一个假百分比。
如果发现「上下文 N%」增长明显偏慢（真实上限是 200K 却按 1M 算了），改 `context_limit`
并重启。

## 系统要求

- **Windows 10 / 11 · x64**。Windows 7 与 32 位系统**不能运行**（exe 静态导入的系统库
  自 Windows 8 起才有）；Windows 8 / 8.1 与 Windows 11 ARM64 未测试。
- 不需要管理员权限；配置写在 `%APPDATA%\claude-hud\`，hook 写在用户目录的 `.claude\settings.json`。
- 界面中文使用**系统自带**的中文字体（Windows 10/11 常规安装均有）。缺少中文字体的环境
  （精简镜像、Server Core 等）启动时会弹框提示，但界面文字会显示为方框。
- 只读取本机 Windows 用户目录下的 `~/.claude/`。Claude Code 跑在 **WSL、远程 SSH 或容器**里时，
  转录与 hook 都在另一套文件系统里，挂件读不到这些会话。
- **不支持 `CLAUDE_CONFIG_DIR`**：hook 与转录路径固定按 `%USERPROFILE%\.claude` 解析。
- 状态与配置**按 Windows 用户隔离**（`%APPDATA%` 与 `%USERPROFILE%`）；换用户或用管理员
  身份另起一个挂件，看到的是另一套会话列表。
- 目前没有开机自启；重复双击会打开两个相同的挂件。

## 升级

1. 用新 exe **覆盖旧文件，位置保持不变** → 重启挂件即可
2. 如果换了目录：新位置要重新执行一次 `--install-hooks`（hook 记的是绝对路径）
3. 升级不需要重开 Claude Code 会话（本版没有改状态文件格式）

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
