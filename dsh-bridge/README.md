# dsh-hud-bridge

把 **DeepSeek Harness**（下称 dsh）的会话事件翻译成 claude-hud 的状态文件。挂件因此能
在一张列表里同时显示 Claude Code 与 dsh 的会话。

桥接运行在 dsh 宿主进程内，只订阅 `ctx` 提供的会话事件、只写 JSON 文件；它不渲染、
不聚合、不判断存活 —— 这些都是 claude-hud 的职责。契约（字段、事件映射、心跳）以仓库
`docs/dsh桥接-方案-2026-11-24.md` 第 4 节为准，本包只负责实现。

## 写到哪里

| 文件 | 内容 |
|---|---|
| `%APPDATA%\claude-hud\sessions\<session_id>.json` | 每个 dsh 会话一份，字段与 Claude 侧状态文件同构，`provider` 为 `"dsh"` |
| `%APPDATA%\claude-hud\dsh-bridge.json` | 心跳：`contract` / `version` / `pid` / `heartbeat_at` / `sessions` |
| `%APPDATA%\claude-hud\dsh-bridge.log` | 运行日志，超过 256 KB 轮转一次（`dsh-bridge.log.1`） |

心跳**不在** `sessions\` 里 —— 放进去会被挂件当成会话文件扫描。

## 哪些会话会出现

**只有此刻在 dsh 里打开着的会话**（方案 §4.4，2026-11-24 用户实机裁定后收紧）：

- 判定来源是宿主服务的 `sessionController.list()`：`agentAvailable === true`（这个会话
  拥有活着的 Agent = 在 dsh 里被打开/在用）或 `running === true`（正在跑）。
- **历史会话**（侧边栏里有、但没打开）、**空白会话**（点了「新建会话」还没说话）都不写。
- 打开 → 下一轮轮询（约 3 秒内）种一条「待命」；关掉 → 下一轮删掉。
- 拿不到 `sessionController` 时退化为「只认事件、不种子」；此时上一次运行留下的
  dsh 状态文件会在约 3 个轮询周期后清掉。

## 事件 → 状态

| dsh 事件 | HUD 状态 |
|---|---|
| `session/created`（新建或恢复） | 待命 |
| `turn/start`、`step/*`、`tool/*` | 工作中 |
| `approval/asked` | 等待确认 |
| `approval/decided` | 回到工作中（该会话还有未答的审批时仍是等待确认） |
| `compaction/start` / `compaction/end` | 压缩中 / 工作中 |
| `turn/end`（`completed` / `interrupted` / 其他） | 完成 / 已打断 / 出错 |
| `session/disposed` | 删除该会话的状态文件 |

`session/title` 只更新标题，`assistant/message` 只更新上下文占用与最后一段正文，
`request/context` 只更新上下文上限；未知事件不改变状态（前向兼容）。

## 手工安装（Phase 1，HUD 尚未内置安装器时）

设 `%APPDATA%` 为 `C:\Users\<你>\AppData\Roaming`、dsh 的 desktop profile 在
`C:\Users\<你>\.dsh\profiles\desktop`。

1. 把整个 `dsh-bridge\` 目录复制到 `%APPDATA%\claude-hud\dsh-bridge\`。
2. 再复制一份到 `<profile>\node_modules\dsh-hud-bridge\`（包名必须可被解析）。
3. 编辑 `<profile>\package.json`（**先备份**）：

   ```json
   "dependencies": {
     "dsh-hud-bridge": "link:C:/Users/<你>/AppData/Roaming/claude-hud/dsh-bridge"
   },
   "dsh": { "profile": { "bundles": [ ..., "dsh-hud-bridge" ] } }
   ```

4. 完全退出并重新打开 DeepSeek Harness。
5. 60 秒内 `%APPDATA%\claude-hud\dsh-bridge.json` 应出现，`sessions\` 里应出现
   `session-*.json`。

失败时的降级路径：用 dsh 设置 → 插件 → 按本地绝对路径安装（官方支持的 `kind: 'path'`）。
若 `<profile>\package.json` 解析失败，**不要**覆盖它 —— 先修好原文件再重试。

## 卸载

删除 `<profile>\package.json` 里的 `dependencies["dsh-hud-bridge"]` 与
`dsh.profile.bundles` 中的 `"dsh-hud-bridge"`，删除
`<profile>\node_modules\dsh-hud-bridge\` 与 `%APPDATA%\claude-hud\dsh-bridge\`，
再删掉 `%APPDATA%\claude-hud\sessions\session-*.json` 与 `dsh-bridge.json`。
其它插件不受影响。

## 已知限制

1. **插件加载前就在跑的那一轮**：种子只写「待命」，下一条事件才会纠正成「工作中」或
   「等待确认」。原因是桥接只依赖注入的 `ctx` 与事件流，不读 dsh 的私有缓存。
   同理，**上一轮的 `context_tokens` 不会跨 dsh 重启保留** —— 新记录从空开始，
   下一条 `assistant/message` 才把它填上。
2. **标题与上下文上限只对「插件加载后」的会话可靠**：`session/title` 与
   `request/context` 都是低频事件（前者在标题生成时发一次，后者在本会话第一次请求 /
   模型变化时发一次）。插件加载前就存在的会话，历史里的这两个事件不会被重放 ⇒ 界面
   回落为文件夹名，上下文上限为空、由 HUD 按全局配置计算。
3. **`context_limit` 不是每个回合都有**：实测同一会话后续回合只发 `request/header`，
   不再发 `request/context`（2026-11-24 真机取证）。所以绝大多数行拿到的是空值，
   这是契约允许的（HUD 侧回落到全局上限），不要把它当 bug 去补默认值。
4. **存活判定**完全由心跳承担：dsh 被强制结束时，HUD 侧在心跳过期（90 秒）后标记
   「已退出」。本包不写 `claude_pid`，也不应写 —— dsh 是 Electron 多进程，会话与进程
   没有一一对应。
5. 只覆盖桌面应用进程。仅 headless / CLI 启动的 dsh 不会被桥接。

## 开发

```sh
npm test        # node --test，纯内置模块，无依赖
```

`src/host.js` 里 `makeRecord` / `reduceEvent` / `contextTokensOf` / `lastAssistantText`
都是纯函数，映射表可以逐条断言；写盘、防抖、心跳、清理在 `startBridge` 里，
单测用临时目录与假 `ctx` 覆盖。
