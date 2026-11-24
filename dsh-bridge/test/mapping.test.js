/**
 * dsh-hud-bridge 单测：事件映射表逐条 + 写盘 / 心跳 / 清理。
 *
 * 跑法：`node --test "test/**\/*.test.js"`（或 `npm test`）。
 * 只用 node 内置模块，不引入测试框架。
 */

import test from 'node:test'
import assert from 'node:assert/strict'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

import {
  CONTRACT,
  contextTokensOf,
  lastAssistantText,
  makeRecord,
  reduceEvent,
  startBridge,
  timeSec,
} from '../src/host.js'

// ───────────────────────────── 夹具 ─────────────────────────────

function session(id = 'session-abc', header = {}) {
  return { id, header: { cwd: 'D:\\dsh_workspace', ...header } }
}

function ev(type, data = {}, timeMs = 1_700_000_000_000) {
  return { type, data, time: timeMs }
}

function record(id = 'session-abc', nowSec = 1000, header = {}) {
  return makeRecord(session(id, header), nowSec)
}

function reduce(rec, event, approvals = new Set(), nowSec = 2000) {
  return reduceEvent(rec, event, nowSec, approvals)
}

function tempDir(tag) {
  return mkdtempSync(join(tmpdir(), `dsh-bridge-${tag}-`))
}

/**
 * 最小 ctx 替身：记录监听、可派发、可卸载。
 *
 * `summaries` 非空时模拟 `sessionController.list()`（`{ sessionId, agentAvailable,
 * running, blank, cwd, origin }`）；传 `null` 模拟「这个服务不存在」的退化路径。
 */
function fakeCtx(summaries = null) {
  const listeners = new Map()
  const cleanups = []
  const ctx = {
    sessions: { list: () => [] },
    get(name) {
      if (name !== 'sessionController' || summaries === null) return undefined
      return { list: async () => ({ items: ctx.summaries }) }
    },
    summaries: summaries ?? [],
    on(type, fn) {
      if (!listeners.has(type)) listeners.set(type, new Set())
      listeners.get(type).add(fn)
      return () => listeners.get(type)?.delete(fn)
    },
    effect(fn) {
      const d = fn()
      if (typeof d === 'function') cleanups.push(d)
    },
    emit(type, ...args) {
      for (const fn of [...(listeners.get(type) ?? [])]) fn(...args)
    },
    dispose() {
      for (const d of cleanups) d()
    },
    listenerCount(type) {
      return listeners.get(type)?.size ?? 0
    },
  }
  return ctx
}

function summary(id, { available = true, running = false, blank = false, cwd = 'D:\\dsh_workspace', origin } = {}) {
  return {
    sessionId: id,
    agentAvailable: available,
    running,
    blank,
    cwd,
    ...(origin === undefined ? {} : { origin }),
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

// ───────────────────────────── 映射表（方案 §4.3 逐条） ─────────────────────────────

test('makeRecord: 初始为 idle，字段齐全，provider 为 dsh', () => {
  const r = record()
  assert.equal(r.session_id, 'session-abc')
  assert.equal(r.cwd, 'D:\\dsh_workspace')
  assert.equal(r.provider, 'dsh')
  assert.equal(r.state, 'idle')
  assert.equal(r.state_since, 1000)
  assert.equal(r.nested, false)
  assert.equal(r.claude_pid, null, 'dsh 不得写自己的 pid —— HUD 的 pid 存活判据不适用')
  assert.equal(r.claude_start, null)
  assert.equal(r.transcript_path, null)
  assert.equal(r.context_tokens, null)
  assert.equal(r.context_limit, null)
})

test('makeRecord: header.origin === subagent ⇒ nested 为真', () => {
  assert.equal(record('session-sub', 1, { origin: 'subagent' }).nested, true)
  assert.equal(record('session-main', 1, {}).nested, false)
  // 只有这个取值是合法的（session-types 校验器同样只认它）
  assert.equal(record('session-x', 1, { origin: undefined }).nested, false)
})

test('turn/start ⇒ working，state_since 取事件时间', () => {
  const r = record()
  reduce(r, ev('turn/start', { turn: 1 }, 1_700_000_005_000))
  assert.equal(r.state, 'working')
  assert.equal(r.state_since, 1_700_000_005)
  assert.equal(r.last_event, 'turn/start')
  assert.equal(r.last_event_at, 1_700_000_005)
})

test('step/*、tool/* 保持 working（不换挡）', () => {
  for (const type of ['step/start', 'step/end', 'tool/call', 'tool/result']) {
    const r = record()
    r.state = 'working'
    r.state_since = 1500
    reduce(r, ev(type, {}, 1_600_000_000_000))
    assert.equal(r.state, 'working', `${type} 不应换挡`)
    assert.equal(r.state_since, 1500, `${type} 不应刷新 state_since`)
  }
})

test('approval/asked ⇒ waiting 并记下待审批 id', () => {
  const r = record()
  const approvals = new Set()
  reduce(r, ev('approval/asked', { id: 'a1', toolName: 'pwsh' }), approvals)
  assert.equal(r.state, 'waiting')
  assert.deepEqual([...approvals], ['a1'])
})

test('approval/decided ⇒ working（没有别的待审批项时）', () => {
  const r = record()
  const approvals = new Set(['a1'])
  r.state = 'waiting'
  reduce(r, ev('approval/decided', { id: 'a1', outcome: 'allowed-once' }), approvals)
  assert.equal(r.state, 'working')
  assert.equal(approvals.size, 0)
})

test('approval/decided：还有别的待审批项时仍是 waiting（不能与事实相反）', () => {
  const r = record()
  const approvals = new Set(['a1', 'a2'])
  r.state = 'waiting'
  reduce(r, ev('approval/decided', { id: 'a1' }), approvals)
  assert.equal(r.state, 'waiting', '还有一个框没答，界面不能写「工作中」')
  assert.deepEqual([...approvals], ['a2'])
})

test('turn/end 清空该会话的待审批集合', () => {
  const r = record()
  const approvals = new Set(['a1'])
  reduce(r, ev('turn/end', { turn: 1, reason: { kind: 'completed' } }), approvals)
  assert.equal(approvals.size, 0)
})

test('turn/end 的三种 reason.kind', () => {
  const cases = [
    ['completed', 'done'],
    ['interrupted', 'interrupted'],
    ['failed', 'error'],
    ['cancelled', 'error'],
    ['', 'error'],
  ]
  for (const [kind, expected] of cases) {
    const r = record()
    reduce(r, ev('turn/end', { turn: 1, reason: { kind } }))
    assert.equal(r.state, expected, `reason.kind=${kind || '(缺失)'}`)
  }
})

test('compaction/start ⇒ compacting；compaction/end ⇒ working', () => {
  const r = record()
  reduce(r, ev('compaction/start'))
  assert.equal(r.state, 'compacting')
  reduce(r, ev('compaction/end'))
  assert.equal(r.state, 'working')
})

test('session/title 只更新 display_name，不改变状态', () => {
  const r = record()
  r.state = 'working'
  reduce(r, ev('session/title', { title: '  鲸鱼娘B站QQ插件配置  ' }))
  assert.equal(r.display_name, '鲸鱼娘B站QQ插件配置', '两端空白必须剥掉')
  assert.equal(r.state, 'working')
})

test('空标题不得把已有标题清掉', () => {
  const r = record()
  r.display_name = '旧标题'
  reduce(r, ev('session/title', { title: '   ' }))
  assert.equal(r.display_name, '旧标题')
})

test('assistant/message ⇒ 上下文 token 与最后一段正文', () => {
  const r = record()
  reduce(
    r,
    ev('assistant/message', {
      usage: { inputTokens: 3731, outputTokens: 779, cacheReadTokens: 37632, cacheWriteTokens: 0 },
      message: {
        role: 'assistant',
        content: [
          { type: 'reasoning', text: '这段话不该出现在副标题里' },
          { type: 'text', text: '配置已写入。' },
        ],
      },
    }),
  )
  assert.equal(r.context_tokens, 41363, 'input + cacheRead + cacheWrite，不含 output')
  assert.equal(r.last_assistant_message, '配置已写入。')
  assert.equal(r.state, 'idle', '这条事件不改变状态')
})

test('request/context ⇒ context_limit', () => {
  const r = record()
  reduce(r, ev('request/context', { provider: 'deepseek-official', model: 'deepseek-v4.1-flash', contextWindow: 1_000_000 }))
  assert.equal(r.context_limit, 1_000_000)
  assert.equal(r.state, 'idle')
})

test('未知事件：不改变状态，只更新 last_event（前向兼容）', () => {
  const r = record()
  r.state = 'working'
  r.state_since = 1500
  reduce(r, ev('future/event-we-do-not-know', { x: 1 }, 1_700_000_009_000))
  assert.equal(r.state, 'working')
  assert.equal(r.state_since, 1500)
  assert.equal(r.last_event, 'future/event-we-do-not-know')
  assert.equal(r.last_event_at, 1_700_000_009)
})

test('job/registered / job/settled 不改变状态', () => {
  const r = record()
  r.state = 'working'
  reduce(r, ev('job/registered', { jobId: 'j1' }))
  reduce(r, ev('job/settled', { jobId: 'j1' }))
  assert.equal(r.state, 'working')
})

// ───────────────────────────── 小工具 ─────────────────────────────

test('timeSec: 毫秒 → 秒，非法值回落', () => {
  assert.equal(timeSec(1_700_000_000_999, 0), 1_700_000_000)
  assert.equal(timeSec(undefined, 42), 42)
  assert.equal(timeSec(Number.NaN, 42), 42)
})

test('contextTokensOf: 三个字段全缺 ⇒ null；有 0 也算已知', () => {
  assert.equal(contextTokensOf(null), null)
  assert.equal(contextTokensOf({}), null)
  assert.equal(contextTokensOf({ inputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0 }), 0)
  assert.equal(contextTokensOf({ inputTokens: 10, cacheReadTokens: 5 }), 15)
})

test('lastAssistantText: 只拼 text 块，忽略 reasoning；超长截断', () => {
  const msg = {
    content: [
      { type: 'reasoning', text: '思路' },
      { type: 'text', text: '第一段' },
      { type: 'text', text: '第二段' },
    ],
  }
  assert.equal(lastAssistantText(msg), '第一段\n第二段')
  assert.equal(lastAssistantText({ content: [] }), '')
  assert.equal(lastAssistantText({}), '')
  const long = { content: [{ type: 'text', text: 'x'.repeat(5000) }] }
  assert.equal(lastAssistantText(long).length, 2000)
})

// ───────────────────────────── 写盘 / 心跳 / 清理 ─────────────────────────────

test('启动后心跳文件出现且字段齐全，且不在 sessions/ 里', async () => {
  const dir = tempDir('heartbeat')
  try {
    const ctx = fakeCtx([])
    const bridge = startBridge(ctx, { appDir: dir, heartbeatMs: 10_000, debounceMs: 5 })
    const hb = JSON.parse(readFileSync(join(dir, 'dsh-bridge.json'), 'utf8'))
    assert.equal(hb.contract, CONTRACT)
    assert.equal(typeof hb.version, 'string')
    assert.equal(hb.pid, process.pid)
    assert.equal(typeof hb.heartbeat_at, 'number')
    assert.equal(hb.sessions, 0)
    assert.ok(!existsSync(join(dir, 'sessions', 'dsh-bridge.json')), '心跳不得放进 sessions/')
    bridge.stop()
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('session/created 本身不写盘（历史会话恢复时也会触发它）', async () => {
  const dir = tempDir('created')
  try {
    const ctx = fakeCtx([]) // sessionController 里没有任何打开的会话
    const bridge = startBridge(ctx, { appDir: dir, heartbeatMs: 10_000, debounceMs: 5, visibilityPollMs: 20 })
    ctx.emit('session/created', session('session-1'))
    ctx.emit('session/event', session('session-1'), ev('turn/start'))
    await sleep(60)
    assert.ok(
      !existsSync(join(dir, 'sessions', 'session-1.json')),
      '没在 sessionController 里打开的会话，一条事件都不该让它出现',
    )
    bridge.stop()
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('只显示 dsh 里真正打开的会话：agentAvailable / running 为真才种', async () => {
  const dir = tempDir('visible')
  try {
    const ctx = fakeCtx([
      summary('session-open', { available: true }),
      summary('session-running', { available: false, running: true }),
      summary('session-history', { available: false, running: false }), // 历史会话
      summary('session-blank', { available: true, blank: true }), // 新建还没说话
    ])
    const bridge = startBridge(ctx, { appDir: dir, heartbeatMs: 10_000, debounceMs: 5, visibilityPollMs: 20 })
    await sleep(20)

    assert.ok(existsSync(join(dir, 'sessions', 'session-open.json')), '打开的会话要显示')
    assert.ok(existsSync(join(dir, 'sessions', 'session-running.json')), '正在跑的会话要显示')
    assert.ok(
      !existsSync(join(dir, 'sessions', 'session-history.json')),
      '没打开的历史会话绝不能出现',
    )
    assert.ok(!existsSync(join(dir, 'sessions', 'session-blank.json')), '空白会话不算打开的工作')
    bridge.stop()
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('会话被关掉（agentAvailable 变假）后，下一轮轮询把行删掉', async () => {
  const dir = tempDir('closed')
  try {
    const ctx = fakeCtx([summary('session-x', { available: true })])
    const bridge = startBridge(ctx, { appDir: dir, heartbeatMs: 10_000, debounceMs: 5, visibilityPollMs: 20 })
    ctx.emit('session/event', session('session-x'), ev('turn/start'))
    await sleep(30)
    assert.ok(existsSync(join(dir, 'sessions', 'session-x.json')))

    ctx.summaries = [summary('session-x', { available: false, running: false })]
    await sleep(80)
    assert.ok(
      !existsSync(join(dir, 'sessions', 'session-x.json')),
      'dsh 里关掉的会话不能留在挂件上',
    )
    bridge.stop()
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('启动时清掉上一次运行留下的 dsh 状态文件，Claude 的一概不动', async () => {
  const dir = tempDir('stale')
  try {
    mkdirSync(join(dir, 'sessions'), { recursive: true })
    writeFileSync(
      join(dir, 'sessions', 'session-stale.json'),
      JSON.stringify({ session_id: 'session-stale', cwd: 'D:\\x', provider: 'dsh', state: 'done', state_since: 1, last_event: 'turn/end', last_event_at: 1 }),
    )
    writeFileSync(
      join(dir, 'sessions', 'claude-1.json'),
      JSON.stringify({ session_id: 'claude-1', cwd: 'D:\\y', state: 'working', state_since: 1, last_event: 'Stop', last_event_at: 1 }),
    )
    const ctx = fakeCtx([summary('session-open', { available: true })])
    const bridge = startBridge(ctx, { appDir: dir, heartbeatMs: 10_000, debounceMs: 5, visibilityPollMs: 20 })
    await sleep(20)

    assert.ok(!existsSync(join(dir, 'sessions', 'session-stale.json')), '残骸必须清掉')
    assert.ok(existsSync(join(dir, 'sessions', 'session-open.json')))
    assert.ok(existsSync(join(dir, 'sessions', 'claude-1.json')), 'Claude 的状态文件一个字都不能动')
    bridge.stop()
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('session/event 经防抖落盘，最终内容是最新状态，且不残留 .tmp', async () => {
  const dir = tempDir('debounce')
  try {
    // 退化模式（没有 sessionController）：事件驱动，方便钉防抖与原子写
    const ctx = fakeCtx(null)
    const bridge = startBridge(ctx, { appDir: dir, heartbeatMs: 10_000, debounceMs: 20, visibilityPollMs: 10_000 })
    const s = session('session-2')
    ctx.emit('session/created', s)
    ctx.emit('session/event', s, ev('turn/start', { turn: 1 }))
    ctx.emit('session/event', s, ev('tool/call', { name: 'pwsh' }, 1_700_000_001_000))
    ctx.emit('session/event', s, ev('turn/end', { turn: 1, reason: { kind: 'completed' } }, 1_700_000_002_000))
    await sleep(120)

    const saved = JSON.parse(readFileSync(join(dir, 'sessions', 'session-2.json'), 'utf8'))
    assert.equal(saved.state, 'done')
    assert.equal(saved.last_event, 'turn/end')
    const leftovers = readdirSync(join(dir, 'sessions')).filter((n) => n.endsWith('.tmp'))
    assert.deepEqual(leftovers, [], '原子写不得残留 tmp')
    bridge.stop()
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('种子状态是 idle，由后续事件纠正', async () => {
  const dir = tempDir('seed')
  try {
    const ctx = fakeCtx([summary('session-open', { available: true })])
    const bridge = startBridge(ctx, { appDir: dir, heartbeatMs: 10_000, debounceMs: 5, visibilityPollMs: 10_000 })
    await sleep(20)
    const seeded = JSON.parse(readFileSync(join(dir, 'sessions', 'session-open.json'), 'utf8'))
    assert.equal(seeded.state, 'idle')
    ctx.emit('session/event', session('session-open'), ev('turn/start'))
    await sleep(40)
    assert.equal(JSON.parse(readFileSync(join(dir, 'sessions', 'session-open.json'), 'utf8')).state, 'working')
    bridge.stop()
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('session/disposed 删除该会话的状态文件', async () => {
  const dir = tempDir('disposed')
  try {
    const ctx = fakeCtx(null) // 退化模式：事件即建立
    const bridge = startBridge(ctx, { appDir: dir, heartbeatMs: 10_000, debounceMs: 5, visibilityPollMs: 10_000 })
    const s = session('session-3')
    ctx.emit('session/created', s)
    ctx.emit('session/event', s, ev('turn/start'))
    await sleep(40)
    assert.ok(existsSync(join(dir, 'sessions', 'session-3.json')))
    ctx.emit('session/disposed', s)
    assert.ok(!existsSync(join(dir, 'sessions', 'session-3.json')))
    bridge.stop()
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('插件卸载：删掉自己的状态文件与心跳，Claude 的一概不动', async () => {
  const dir = tempDir('unload')
  try {
    const ctx = fakeCtx(null)
    const bridge = startBridge(ctx, { appDir: dir, heartbeatMs: 10_000, debounceMs: 5, visibilityPollMs: 10_000 })
    const s4 = session('session-4')
    ctx.emit('session/created', s4)
    ctx.emit('session/event', s4, ev('turn/start'))
    await sleep(40)
    writeFileSync(
      join(dir, 'sessions', 'claude-2.json'),
      JSON.stringify({ session_id: 'claude-2', cwd: 'D:\\y', state: 'working', state_since: 1, last_event: 'Stop', last_event_at: 1 }),
    )

    ctx.dispose() // 模拟 cordis 卸载插件

    assert.ok(!existsSync(join(dir, 'sessions', 'session-4.json')))
    assert.ok(!existsSync(join(dir, 'dsh-bridge.json')))
    assert.ok(existsSync(join(dir, 'sessions', 'claude-2.json')), 'Claude 会话必须不受影响')
    bridge.stop() // 幂等
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('会话对象缺 id 时安静跳过，不写文件', async () => {
  const dir = tempDir('noid')
  try {
    const ctx = fakeCtx(null)
    const bridge = startBridge(ctx, { appDir: dir, heartbeatMs: 10_000, debounceMs: 5, visibilityPollMs: 10_000 })
    ctx.emit('session/created', { header: { cwd: 'D:\\x' } })
    ctx.emit('session/event', { header: {} }, ev('turn/start'))
    assert.deepEqual(readdirSync(join(dir, 'sessions')), [])
    bridge.stop()
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('事件处理抛异常不会穿透到 dsh', async () => {
  const dir = tempDir('throw')
  try {
    const ctx = fakeCtx(null)
    const bridge = startBridge(ctx, { appDir: dir, heartbeatMs: 10_000, debounceMs: 5, visibilityPollMs: 10_000 })
    // data 是 getter，读它就抛 —— 桥接必须自己吞掉
    const evil = {
      type: 'turn/start',
      time: 1_700_000_000_000,
      get data() {
        throw new Error('boom')
      },
    }
    assert.doesNotThrow(() => ctx.emit('session/event', session('session-5'), evil))
    bridge.stop()
  } finally {
    rmSync(dir, { recursive: true, force: true })
  }
})

test('APPDATA 不可用时停用而不抛', async () => {
  const saved = process.env.APPDATA
  delete process.env.APPDATA
  try {
    const ctx = fakeCtx([])
    const bridge = startBridge(ctx, {})
    assert.equal(bridge.disabled, true)
    assert.doesNotThrow(() => bridge.stop())
  } finally {
    if (saved !== undefined) process.env.APPDATA = saved
  }
})
