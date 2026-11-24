/**
 * dsh-hud-bridge —— 把 DeepSeek Harness（下称 dsh）的会话事件翻译成 claude-hud 的状态文件。
 *
 * 契约见仓库 `docs/dsh桥接-方案-2026-11-24.md` 第 4 节；本文件只实现契约，不定义契约。
 *
 * 三条硬约束（方案 §5.2）：
 *   1. 只用注入的 `ctx`，**不 import 任何 `@deepseek-ai/*` 包** —— 不依赖应用内部的模块解析；
 *   2. 所有回调包在 try/catch 里，异常只写日志，绝不向 dsh 主进程抛；
 *   3. 不注册 UI、不监听端口、不发网络请求。
 *
 * 另有一条本文件自己的纪律：**映射必须是纯函数**（`reduceEvent` / `makeRecord`）。
 * 写盘、防抖、心跳、清理都在 `startBridge` 里，单测可以直接喂事件断言映射表逐条成立。
 */

import {
  mkdirSync,
  readdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

export const name = 'hud-bridge'
export const inject = ['sessions', 'timer']

/** 契约版本。HUD 侧支持不了这个值时必须明示「桥接版本不兼容」。 */
export const CONTRACT = 1
/** 心跳周期（毫秒）；HUD 侧过期判据是 90 秒（允许连丢 6 次）。 */
export const HEARTBEAT_MS = 15_000
/** 单会话落盘防抖（毫秒）。 */
export const DEBOUNCE_MS = 200
/** 日志单文件上限；超限轮转一次（`dsh-bridge.log.1`）。 */
const LOG_MAX_BYTES = 256 * 1024
/** `last_assistant_message` 只用于一行副标题，截断避免把状态文件撑大。 */
const LAST_MESSAGE_MAX = 2000

/** 包版本。HUD 安装时会把 package.json 的 version 改写成自己的版本号（方案 §7.4）。 */
const VERSION = (() => {
  try {
    const pkg = fileURLToPath(new URL('../package.json', import.meta.url))
    return String(JSON.parse(readFileSync(pkg, 'utf8')).version ?? '0.0.0')
  } catch {
    return '0.0.0'
  }
})()

// ───────────────────────────── 纯映射（可单测） ─────────────────────────────

/** 事件里的时间戳是毫秒；状态文件用秒（与 Claude 侧一致）。 */
export function timeSec(ms, fallbackSec) {
  return Number.isFinite(ms) ? Math.floor(ms / 1000) : fallbackSec
}

/**
 * 由会话对象造一条初始记录。只读 `session.id` 与 `session.header`，
 * 不碰任何私有缓存、不 import 应用内部包。
 *
 * `nested` 口径与 Claude 侧一致：`Some(true)` 表示子会话，HUD 不列出来。
 */
export function makeRecord(session, nowSec) {
  const header = session?.header ?? {}
  return {
    session_id: String(session?.id ?? ''),
    cwd: typeof header.cwd === 'string' ? header.cwd : '',
    display_name: null,
    provider: 'dsh',
    state: 'idle',
    state_since: nowSec,
    last_event: 'session/created',
    last_event_at: nowSec,
    nested: header.origin === 'subagent',
    context_tokens: null,
    context_limit: null,
    transcript_path: null,
    claude_pid: null,
    claude_start: null,
    last_assistant_message: null,
    notification_message: null,
    transcript_offset: 0,
  }
}

/** 只有状态值真的变了才刷新 `state_since`（Claude 侧同一口径）。 */
function setState(rec, state, at) {
  if (rec.state === state) return
  rec.state = state
  rec.state_since = at
}

/** 上下文占用 = 非缓存输入 + 缓存读 + 缓存写（与方案 §4.1 一致；不含输出）。 */
export function contextTokensOf(usage) {
  if (!usage || typeof usage !== 'object') return null
  const n = ['inputTokens', 'cacheReadTokens', 'cacheWriteTokens']
    .map((k) => (Number.isFinite(usage[k]) ? usage[k] : 0))
    .reduce((a, b) => a + b, 0)
  // 三个字段全缺 ⇒ 没有事实，留空；全是 0 是合法的（刚开的会话）
  const known = ['inputTokens', 'cacheReadTokens', 'cacheWriteTokens'].some((k) =>
    Number.isFinite(usage[k]),
  )
  return known ? n : null
}

/** 只取 `type === 'text'` 的块拼成一行；reasoning 不进副标题。 */
export function lastAssistantText(message) {
  const content = message?.content
  if (!Array.isArray(content)) return ''
  const text = content
    .filter((b) => b && b.type === 'text' && typeof b.text === 'string')
    .map((b) => b.text)
    .join('\n')
    .trim()
  return text.length > LAST_MESSAGE_MAX ? text.slice(0, LAST_MESSAGE_MAX) : text
}

function turnEndKind(data) {
  const reason = data?.reason
  if (typeof reason === 'string') return reason
  if (reason && typeof reason === 'object') return String(reason.kind ?? '')
  return ''
}

/**
 * 事件 → 记录（原地修改并返回）。映射表见方案 §4.3；未知事件只更新
 * `last_event` / `last_event_at`，**不改变状态** —— 前向兼容，dsh nightly
 * 加了新事件也不会把界面搞乱。
 *
 * `approvals` 是该会话的待审批 id 集合，由调用方持有（跨事件、跨重写）。
 */
export function reduceEvent(rec, event, nowSec, approvals) {
  const type = String(event?.type ?? '')
  const data = event?.data ?? {}
  const at = timeSec(event?.time, nowSec)

  rec.last_event = type
  rec.last_event_at = at

  switch (type) {
    case 'turn/start':
      setState(rec, 'working', at)
      break

    case 'step/start':
    case 'step/end':
    case 'tool/call':
    case 'tool/result':
      // 方案 §4.3：这些事件「保持 working」。它们是同一轮里的推进，不换挡。
      setState(rec, 'working', at)
      break

    case 'approval/asked': {
      const id = String(data.id ?? '')
      if (id) approvals?.add(id)
      setState(rec, 'waiting', at)
      break
    }

    case 'approval/decided': {
      const id = String(data.id ?? '')
      if (id) approvals?.delete(id)
      // 方案写的是「→ working」。多个审批并存时，只要还有没答的，界面就必须
      // 继续显示「等待确认」—— 显示 working 与事实相反，正是挂件要避免的失效。
      setState(rec, approvals?.size > 0 ? 'waiting' : 'working', at)
      break
    }

    case 'compaction/start':
      setState(rec, 'compacting', at)
      break

    case 'compaction/end':
      setState(rec, 'working', at)
      break

    case 'turn/end': {
      // 一轮结束 ⇒ 该会话不再有待审批项（方案 §4.3）
      approvals?.clear()
      const kind = turnEndKind(data)
      setState(rec, kind === 'completed' ? 'done' : kind === 'interrupted' ? 'interrupted' : 'error', at)
      break
    }

    case 'session/title': {
      const title = typeof data.title === 'string' ? data.title.trim() : ''
      if (title) rec.display_name = title
      break
    }

    case 'assistant/message': {
      const tokens = contextTokensOf(data.usage)
      if (tokens !== null) rec.context_tokens = tokens
      const text = lastAssistantText(data.message)
      if (text) rec.last_assistant_message = text
      break
    }

    case 'request/context': {
      const limit = Number(data.contextWindow)
      if (Number.isFinite(limit) && limit > 0) rec.context_limit = limit
      break
    }

    default:
      // 未知事件：不改变状态，只留下 last_event 供诊断
      break
  }

  return rec
}

// ───────────────────────────── 运行时 ─────────────────────────────

function defaultAppDir() {
  const base = process.env.APPDATA
  return base ? join(base, 'claude-hud') : null
}

/**
 * 启动桥接。返回 `{ stop }`；`ctx.effect` 卸载时会自动调用 `stop`。
 *
 * `opts` 只为测试而存在：`appDir`（临时目录）、`now` / `heartbeatMs` / `debounceMs`
 * （假时钟与短周期）。生产路径全部走默认值。
 */
export function startBridge(ctx, opts = {}) {
  const appDir = opts.appDir ?? defaultAppDir()
  const now = opts.now ?? (() => Date.now())
  const heartbeatMs = opts.heartbeatMs ?? HEARTBEAT_MS
  const debounceMs = opts.debounceMs ?? DEBOUNCE_MS
  const nowSec = () => Math.floor(now() / 1000)

  if (!appDir) {
    // 取不到 %APPDATA% 时停用（方案 §5.3），但绝不抛 —— 不能让桥接拖垮 dsh
    return { stop() {}, disabled: true }
  }

  const sessionsDir = join(appDir, 'sessions')
  const heartbeatPath = join(appDir, 'dsh-bridge.json')
  const logPath = join(appDir, 'dsh-bridge.log')
  let tmpSeq = 0

  try {
    mkdirSync(sessionsDir, { recursive: true })
  } catch (error) {
    return { stop() {}, disabled: true, error }
  }

  function log(line) {
    try {
      try {
        if (statSync(logPath).size > LOG_MAX_BYTES) renameSync(logPath, `${logPath}.1`)
      } catch {
        // 日志不存在或轮转失败：都不该影响写盘
      }
      writeFileSync(logPath, `[${new Date(now()).toISOString()}] ${line}\n`, { flag: 'a' })
    } catch {
      // 日志写不进去就算了
    }
  }

  /** 先写唯一名 tmp 再 rename —— 与 HUD 状态文件同一约定，读侧永远看不到半截 JSON。 */
  function atomicWrite(target, text) {
    const tmp = `${target}.${process.pid}.${tmpSeq++}.tmp`
    writeFileSync(tmp, text)
    try {
      renameSync(tmp, target)
    } catch (error) {
      try {
        rmSync(tmp)
      } catch {
        // tmp 清不掉：留待下次启动的同名覆盖，不阻断调用方
      }
      throw error
    }
  }

  /** id → { rec, approvals:Set, timer } */
  const records = new Map()
  let stopped = false

  /**
   * **可见集合** = dsh 里此刻真正打开着的会话（`sessionController.list()` 里
   * `agentAvailable === true`，或正在 `running`）。
   *
   * 为什么不能直接拿 `ctx.sessions.list()`：那是「进入了 Store 的会话」，包括历史
   * 标签、插件（如 QQ 机器人）在后台建的会话 —— 2026-11-24 用户实机指出这些不该
   * 出现在挂件上。`sessionController.list()` 的 `agentAvailable` 才是「这个会话此刻
   * 拥有活着的 Agent」= 在 dsh 里被打开/在用。
   *
   * `null` = 拿不到这个服务 ⇒ 退化成事件驱动（不种子、只认事件），绝不让桥接挂掉。
   */
  let openIds = null
  let pollTimer = null
  let soonTimer = null
  let pollInFlight = false

  function controller() {
    try {
      return ctx?.get?.('sessionController') ?? ctx?.sessionController
    } catch {
      return undefined
    }
  }

  function isVisible(id) {
    return openIds === null || openIds.has(id)
  }

  function cleanupStale(keep) {
    for (const name of listDshStateFiles()) {
      const id = name.slice(0, -'.json'.length)
      if (keep.has(id)) continue
      try {
        rmSync(join(sessionsDir, name))
      } catch {
        // 删不掉就留着，下一轮还有机会
      }
    }
  }

  /**
   * 把一次 `sessionController.list()` 的结果落到本地：新打开的会话种一条待命，
   * 不再打开的会话连文件一起删掉。历史会话（从未打开）从一开始就不会被种。
   */
  function applyOpenSet(next, items) {
    const before = openIds
    openIds = next
    const byId = new Map()
    for (const item of items) {
      const id = String(item?.sessionId ?? item?.id ?? '')
      if (id) byId.set(id, item)
    }

    for (const id of next) {
      if (records.has(id)) continue
      const item = byId.get(id) ?? {}
      const session = { id, header: { cwd: item.cwd, origin: item.origin } }
      const entry = entryFor(session)
      if (!entry) continue
      flush(entry)
      log(`会话打开 ${id}`)
    }

    for (const [id, entry] of [...records]) {
      if (next.has(id)) continue
      if (entry.timer) clearTimeout(entry.timer)
      records.delete(id)
      try {
        rmSync(statePath(id))
      } catch {
        // 文件不在就正好
      }
      log(`会话关闭 ${id}`)
    }

    const changed =
      before === null ||
      next.size !== before.size ||
      [...next].some((id) => !before.has(id))
    if (changed) cleanupStale(next)
  }

  let degradedTicks = 0
  let degradedCleaned = false

  async function refreshOpenSessions() {
    const sc = controller()
    if (!sc || typeof sc.list !== 'function') {
      // 服务可能比插件晚挂载：连等三次（约 3 个周期）再认定退化，避免误删刚种下的行
      degradedTicks += 1
      if (openIds !== null) {
        openIds = null
        log('取不到 sessionController，退化为事件驱动')
      }
      if (!degradedCleaned && degradedTicks >= 3) {
        degradedCleaned = true
        degradedStartupCleanup()
      }
      return
    }
    degradedTicks = 0
    if (pollInFlight || stopped) return
    pollInFlight = true
    try {
      const res = await sc.list({})
      if (stopped) return
      const items = Array.isArray(res) ? res : (res?.items ?? [])
      const next = new Set()
      for (const item of items) {
        const id = String(item?.sessionId ?? item?.id ?? '')
        if (!id) continue
        // 空白会话（点了「新建会话」还没说话）不是「打开的工作」
        if (item.blank === true) continue
        if (item.agentAvailable === true || item.running === true) next.add(id)
      }
      applyOpenSet(next, items)
    } catch (error) {
      log(`读取会话列表失败: ${error?.message ?? error}`)
    } finally {
      pollInFlight = false
    }
  }

  /** 打开/新建会话后尽快刷新一次，别等下一个 3 秒周期。 */
  function pollSoon() {
    if (soonTimer || stopped) return
    soonTimer = setTimeout(() => {
      soonTimer = null
      refreshOpenSessions()
    }, 100)
    soonTimer.unref?.()
  }

  function statePath(id) {
    return join(sessionsDir, `${id}.json`)
  }

  function flush(entry) {
    if (stopped) return
    try {
      atomicWrite(statePath(entry.rec.session_id), JSON.stringify(entry.rec, null, 2))
    } catch (error) {
      log(`写状态文件失败 ${entry.rec.session_id}: ${error?.message ?? error}`)
    }
  }

  function schedule(entry) {
    if (entry.timer) return
    entry.timer = setTimeout(() => {
      entry.timer = null
      flush(entry)
    }, debounceMs)
    entry.timer.unref?.()
  }

  function entryFor(session) {
    const id = String(session?.id ?? '')
    if (!id || !isVisible(id)) return null
    let entry = records.get(id)
    if (!entry) {
      entry = { rec: makeRecord(session, nowSec()), approvals: new Set(), timer: null }
      records.set(id, entry)
    }
    return entry
  }

  function handleCreated() {
    // 这里**不写盘**：历史会话被恢复时也会触发 session/created，它们不该出现在挂件上。
    // 只顺势刷新一次可见集合；真正的可见性由 sessionController 的 agentAvailable 决定。
    pollSoon()
  }

  function handleEvent(session, event) {
    const entry = entryFor(session)
    if (!entry) {
      // 没见过的会话：可能刚在 UI 里打开，马上刷一次可见集合
      if (event?.type === 'turn/start') pollSoon()
      return
    }
    reduceEvent(entry.rec, event, nowSec(), entry.approvals)
    schedule(entry)
  }

  function handleDisposed(session) {
    const id = String(session?.id ?? '')
    const entry = records.get(id)
    if (entry?.timer) clearTimeout(entry.timer)
    records.delete(id)
    try {
      rmSync(statePath(id))
    } catch {
      // 文件不在就正好
    }
    log(`会话结束 ${id}`)
  }

  function writeHeartbeat() {
    if (stopped) return
    try {
      atomicWrite(
        heartbeatPath,
        JSON.stringify(
          {
            contract: CONTRACT,
            version: VERSION,
            pid: process.pid,
            heartbeat_at: nowSec(),
            sessions: records.size,
          },
          null,
          2,
        ),
      )
    } catch (error) {
      log(`写心跳失败: ${error?.message ?? error}`)
    }
  }

  /**
   * 退化模式（拿不到 `sessionController`）：不种子，只认事件；并把上一次运行
   * 留下的 dsh 状态文件全部清掉 —— 没有可见性服务时，宁可少显示也不留历史行。
   */
  function degradedStartupCleanup() {
    cleanupStale(new Set(records.keys()))
    log('桥接已启动（退化模式：只认事件，不种子历史会话）')
  }

  /** 只认自己写下的文件（`provider === 'dsh'`），绝不碰 Claude 的状态文件。 */
  function listDshStateFiles() {
    try {
      return readdirSync(sessionsDir).filter((name) => {
        if (!name.endsWith('.json')) return false
        try {
          const text = readFileSync(join(sessionsDir, name), 'utf8')
          return JSON.parse(text).provider === 'dsh'
        } catch {
          return false
        }
      })
    } catch {
      return []
    }
  }

  // ── 启动：先落心跳，再刷可见集合 ──
  writeHeartbeat()
  refreshOpenSessions()
  writeHeartbeat()
  const heartbeatTimer = setInterval(writeHeartbeat, heartbeatMs)
  heartbeatTimer.unref?.()
  const visibilityPollMs = opts.visibilityPollMs ?? 3000
  pollTimer = setInterval(refreshOpenSessions, visibilityPollMs)
  pollTimer.unref?.()

  function stop() {
    if (stopped) return
    stopped = true
    clearInterval(heartbeatTimer)
    if (pollTimer) clearInterval(pollTimer)
    if (soonTimer) clearTimeout(soonTimer)
    for (const entry of records.values()) {
      if (entry.timer) clearTimeout(entry.timer)
    }
    // 卸载即清场：dsh 会话随插件一起消失（方案 §5.3）
    for (const name of listDshStateFiles()) {
      try {
        rmSync(join(sessionsDir, name))
      } catch {
        // 下一次启动会把它当残骸删掉
      }
    }
    records.clear()
    try {
      rmSync(heartbeatPath)
    } catch {
      // 不在就正好
    }
    log('桥接已卸载')
  }

  // 所有监听都包在 try/catch 里：dsh 侧任何一条事件都不该因为桥接出错而失败
  const safe = (fn) => (...args) => {
    try {
      fn(...args)
    } catch (error) {
      log(`事件处理异常: ${error?.message ?? error}`)
    }
  }

  const offs = []
  try {
    offs.push(ctx.on('session/created', safe(handleCreated)))
    offs.push(ctx.on('session/event', safe(handleEvent)))
    offs.push(ctx.on('session/disposed', safe(handleDisposed)))
  } catch (error) {
    log(`注册会话监听失败: ${error?.message ?? error}`)
  }

  if (typeof ctx.effect === 'function') {
    ctx.effect(() => () => {
      for (const off of offs) {
        try {
          off?.()
        } catch {
          // 解绑失败不影响下面清场
        }
      }
      stop()
    }, 'dsh-hud-bridge')
  }

  return { stop }
}

/** cordis 入口。 */
export function apply(ctx) {
  return startBridge(ctx)
}
