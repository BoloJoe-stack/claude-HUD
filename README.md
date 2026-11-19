# claude-hud

Windows 桌面常驻组件，用于同时查看多个并发 Claude Code 会话的运行状态：工作中、等待确认、
完成、被打断、出错。数据来源限于本机文件，即 hook 写入的 `%APPDATA%\claude-hud\sessions\`
与 `~/.claude/` 下的转录；程序不进行网络通信。

## 安装

1. 从 [Releases](https://github.com/BoloJoe-stack/claude-HUD/releases) 下载 `claude-hud.exe`，
   存放于后续不再移动的目录。
   - 仅需单个 exe；无需安装 Rust、VC++ 运行库、.NET 或 Node（C 运行库已静态链接至 exe，
     在标准 Windows 环境中可直接运行）。
   - 请勿在 zip 压缩包内直接运行：Windows 会将 exe 解压至临时目录后执行，hook 记录的是该
     临时路径，系统重启后即失效。
2. 双击 `claude-hud.exe`。首次启动时将询问是否注册 8 个 hook，选择「是」。注册写入全局
   `~/.claude/settings.json`，仅新增 `hooks` 键，原有 `env` 等配置不作改动；后续启动不再
   询问。亦可手动执行 `claude-hud.exe --install-hooks`。
3. 重启已打开的 Claude Code 会话。hook 配置仅在会话启动时加载；安装 hook 之前已打开的
   会话不会补发事件，挂件中不会显示。此后新开的会话正常显示。

确认生效：顶栏显示 `● claude 会话 · N 个`，N 与实际会话数一致。若显示 `0 个` 或
「没有活跃会话」，按「hook 未生效的排查」逐项核对。

| 命令 | 作用 |
|---|---|
| `claude-hud.exe --uninstall-hooks` | 卸载 hook |
| `claude-hud.exe --doctor` | 自检 |
| `claude-hud.exe --version` | 显示版本 |

**注意**

- `--install-hooks` 会改写**全局** settings，影响所有 Claude Code 会话；执行前建议备份。
- hook 记录 exe 的**绝对路径**；移动 exe 之后须重新执行 `--install-hooks`。
- 本项目**未做代码签名**。从浏览器下载的 exe 首次运行时，Windows SmartScreen 将提示
  「Windows 已保护你的电脑」，选择「更多信息 → 仍要运行」即可继续；可用 Release 正文中的
  SHA256 校验文件完整性。

## 会话数的来源

挂件**不扫描正在运行的进程**，仅读取 8 个 hook 写入的状态文件：
`%APPDATA%\claude-hud\sessions\<session_id>.json`。由此产生以下行为：

- 一行对应一个**会话**，不对应一个窗口；同一窗口中的两个终端计为两个会话。
- 由其他会话以 `claude -p` 拉起的子会话不单独列出，其工作量计入父会话所在行。
- hook 未生效时不存在状态文件，顶栏显示 `0 个`。
- 会话在安装 hook **之前**已打开时不存在状态文件，故不显示；重新打开一次即可。

**自动缩放**：会话条数变化时，窗口高度调整为恰好容纳内容，宽度与左上角位置保持不变；
手动拖动或缩放期间不介入；首轮扫描完成前不调整；手动调整后的高度不会被每帧重置。

## hook 未生效的排查

按以下顺序核对：

1. 运行 `claude-hud.exe --doctor`，检查两项输出：
   - 「hook 未注册（读不到 settings.json）」「hook 缺 N 个事件」
     「hook 指向的 exe 不存在 —— 8 个事件都会静默失效」
   - 「状态目录不存在 —— hook 从来没成功写过东西」
     「最近 24 小时没有任何 hook 写过状态文件」
2. 打开 `%USERPROFILE%\.claude\settings.json` 的 `hooks`，确认 8 个事件（`SessionStart` /
   `UserPromptSubmit` / `Notification` / `PreCompact` / `PostCompact` / `Stop` /
   `StopFailure` / `SessionEnd`）的 `command` 是否为当前 exe 的绝对路径。移动 exe 或更换
   版本目录会导致该路径失效，重新执行 `--install-hooks` 即可。
3. **重启 Claude Code 会话**后再次查看（执行安装步骤 3 之后未重启，老会话不会出现）。
4. 检查 `%APPDATA%\claude-hud\sessions\` 中的 `.json` 文件是否增加；不增加表示 hook 未执行。
5. 设置了 `CLAUDE_CONFIG_DIR` 的机器：当前版本仅识别 `%USERPROFILE%\.claude`，**不支持**
   自定义配置目录，表现为 0 个会话。
6. Claude Code 运行于 WSL、远程 SSH、容器或另一 Windows 用户下时，hook 与转录位于另一套
   文件系统，挂件无法读取（见「系统要求」）。

## 配置

配置文件为 `%APPDATA%\claude-hud\config.json`；文件不存在时程序使用默认值，照常运行。

| 字段 | 默认 | 说明 |
|---|---|---|
| `context_limit` | **未设置** | 上下文上限。首次运行询问一次；未设置时仅显示 token 数，不显示百分比 |
| `warn_threshold` | `50` | 占比达到此值转为琥珀色 |
| `danger_threshold` | `75` | 占比达到此值转为红色 |
| `poll_interval_ms` | `1000` | 轮询周期，单位毫秒 |
| `idle_after_sec` | `300` | 完成多少秒后显示为「待命」 |
| `always_on_top` | `true` | 窗口置顶 |
| `window_pos` | `[100, 100]` | 窗口位置，拖动后自动记录 |

**首次运行询问上下文上限的原因**：会话文件仅记录 model ID，不包含窗口大小字段，上限无法
从文件推断；按 model ID 推测在部分模型上会产生数倍误差，且显示数值看似合理，无法察觉。
因此程序不作推测，仅询问一次：**是 = 1,000,000 / 否 = 200,000 / 取消 = 暂不设置**。设置
有误时修改 `config.json` 并重启挂件。

**未设置时的行为**：第二行显示 `上下文 --` 与 token 数，**不**按默认值计算百分比。若
「上下文 N%」的增长慢于预期（实际上限为 200K 而按 1M 计算），修改 `context_limit` 后
重启挂件。

## 系统要求

- **Windows 10 / 11 · x64**。Windows 7 与 32 位系统**无法运行**（exe 静态导入的系统库自
  Windows 8 起提供）；Windows 8 / 8.1 与 Windows 11 ARM64 未经测试。
- 无需管理员权限；配置写入 `%APPDATA%\claude-hud\`，hook 写入用户目录的
  `.claude\settings.json`。
- 界面中文使用**系统自带**的中文字体（Windows 10/11 常规安装均包含）。缺少中文字体的
  环境（精简镜像、Server Core 等）启动时会弹框提示，但界面文字显示为方框。
- 仅读取本机 Windows 用户目录下的 `~/.claude/`。Claude Code 运行于 **WSL、远程 SSH 或
  容器**时，转录与 hook 位于另一套文件系统，挂件无法读取这些会话。
- **不支持 `CLAUDE_CONFIG_DIR`**：hook 与转录路径固定按 `%USERPROFILE%\.claude` 解析。
- 状态与配置**按 Windows 用户隔离**（`%APPDATA%` 与 `%USERPROFILE%`）；切换用户或以
  管理员身份另启挂件时，显示的是另一套会话列表。
- 当前版本无开机自启；重复双击将启动两个相同的挂件。

## 升级

1. 用新 exe **覆盖旧文件，位置保持不变**，重启挂件即可。
2. 更换目录时，须在新位置重新执行一次 `--install-hooks`（hook 记录绝对路径）。
3. 本版未变更状态文件格式，升级无需重开 Claude Code 会话。

## 从源码构建

需要 Rust（`stable-x86_64-pc-windows-msvc`）：

```bash
cargo build --release      # → target/release/claude-hud.exe
```

仅支持 Windows 10/11 x64；exe 无运行时依赖（无需 .NET / WebView / Node）。

仓库中的 `.cargo/config.toml` 为 MSVC 目标添加 `+crt-static`，**不得删除**。缺少该配置时，
Rust 默认动态链接 `VCRUNTIME140.dll`，在未安装 VC++ 运行库的机器上会报
"计算机中丢失 VCRUNTIME140.dll"。修改构建配置后，用以下命令自检，输出中**不得**出现
`VCRUNTIME140.dll`：

```bash
objdump -p target/release/claude-hud.exe | grep "DLL Name"
```

## 许可

- 本项目采用 **MIT** 许可，见 `LICENSE`。
- 随二进制打包的 `assets/claude-hud-digits.ttf` 为 **DSEG14 的数字子集**（© keshikan，
  SIL OFL 1.1；按 OFL 条件 3 已改名），完整声明见 `assets/` 下两个文件。
- 中文与拉丁字母使用系统自带的微软雅黑，不随包分发。

## 更多

逐项决策、实测数据与已知问题见 [`docs/工作日志.md`](docs/工作日志.md)。
