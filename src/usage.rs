//! 今日 token 账本 —— **纯函数**，不做任何 I/O。
//!
//! 用户 2026-09-16 的需求：顶栏显示"所有项目今天消耗的 token"，会话行尾显示"这条会话今天用了多少"。
//! 计划见 `docs/superpowers/plans/2026-09-16-token-and-subagent-link.md`（本模块是它的 T1）。
//!
//! 本模块只回答一个问题：**给你一批转录行，今天一共用了多少、每个项目多少**。
//! 谁来读文件、什么时候读、读多少，全在 `poller` / 未来的扫描器里 —— 分开是为了让
//! "算账"这件事可以脱离文件系统被钉死（跨零点、跨文件去重这些**最容易错**的地方都在这里）。
//!
//! ## 三条硬要求（缺一条数字就是错的，全部是实测出来的）
//!
//! 1. **四个桶平铺相加**（`input` + `output` + `cache_read` + `cache_creation`）。
//!    用户明确"不要钱，只要 token 数"。代价已知情：这个数 ≈ 99.8% 来自 `cache_read`，
//!    它量的是"同一段上下文被重复读了多少遍"，不是"新读进去多少内容"。
//!    ⚠️ **桶会缺**：实测全机 42,710 行 assistant 里 **12.9% 只有 `input`/`output` 两个桶**
//!    （历史版本的转录**根本没有 cache 那两个键**，不是 0）。缺桶按 0 算，**不许整行丢掉**。
//! 2. **按行内 `timestamp` 判"今天"**，不按文件 mtime —— 一个昨天开、今天还在跑的会话，
//!    文件是今天的，里面却混着昨天的行。
//! 3. ⭐ **按 `message.id` 归组，组内只算一行**（**不是**按 `uuid` 去重）。
//!    **一次 API 响应会写成多行**：`thinking` / `text` / `tool_use` 各一行，**每行 uuid 都不同**
//!    而 `message.id` 相同、`usage` 基本一样。按行累加会把同一次调用算好几遍 ——
//!    实测今天 **2245 行只对应 965 个 `message.id`**，按行累加 403.2M、按组取一行 175.8M，
//!    **虚高 2.29 倍**。（`uuid` 去重救不了这个：那三行的 uuid 本来就不一样。）
//!
//!    组内取**`timestamp` 最大的那一行**：实测全机 19,799 组里，最后一行的 `output_tokens`
//!    **100% 是组内最大值**（零反例）。中途的行会写着 `output_tokens: 0`（那一刻还没生成完）。
//!    注意两件事：① "最后一行在每个桶上都最大"**不成立**（只有 5,890/9,155 组成立，
//!    `thinking` 行的 `input_tokens` 可能更大）—— 但那是同一次请求的中间态，取最终值是对的；
//!    ② 有 394 组的**文件顺序与时间戳顺序不一致**，所以判据取"时间戳最大"而不是"最后读到的那行"。
//!
//!    `message.id` 有两种写法（裸 UUID 与 `msg_` 前缀），**解析时别假设格式**；
//!    万一某行没有它，退回用 `uuid` 当键（绝不因为缺字段就把这行算两遍）。
//!
//! ## 一处必须记住的口径：项目是**按行**取的
//!
//! 实测 **79 / 163 份转录含多个 `cwd`**（最多 13 个）—— "这个会话属于哪个项目"**没有唯一解**，
//! 所以项目归属只能取**每一行自己的 `cwd`**，而不能挂在会话上。

use std::collections::BTreeMap;

use serde_json::Value;

/// 计入总量的四个桶。**顺序无关，但缺一个数字就错**。
pub const BUCKETS: [&str; 4] = [
    "input_tokens",
    "output_tokens",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
];

/// 一行里能用来算账的四样东西（其余字段一律不要 —— 账本越窄越难写错）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageLine {
    /// 去重键（兜底用）。**没有 uuid 的行直接丢弃**：算不了去重就等于给总数注水。
    pub uuid: String,
    /// **一次 API 响应的身份**（转录里的 `message.id`）。组内只算一行，见模块头的第 3 条。
    /// 拿不到时为 `None`，那时退回用 `uuid` 当键。
    pub msg_id: Option<String>,
    /// 行内 `timestamp` 的 epoch 秒（UTC 原样，**判"今天"时再换本地**）。
    pub at: i64,
    /// 四个桶之和。
    pub tokens: u64,
    /// 行内 `cwd`（项目归属，**按行取**）。
    pub cwd: String,
}

/// 为什么没有 `uuid` 的行要丢：它是去重的唯一依据。
///
/// 转录行的实际形态（真机）：`{"type":"assistant","timestamp":"…Z","uuid":"…",
/// "cwd":"…","message":{"usage":{…}}}`。`type` 必须是 `assistant` —— 只有助手消息带 `usage`。
pub fn parse_usage_line(line: &str) -> Option<UsageLine> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let uuid = v.get("uuid").and_then(|u| u.as_str())?.to_string();
    // 时间戳解析失败就丢：**宁少算一行，也不把一行算到错误的日子里**（跨零点的数字会全歪）。
    let at = super::transcript::parse_ts(v.get("timestamp").and_then(|t| t.as_str())?)?;
    let msg = v.get("message")?;
    let usage = msg.get("usage")?;
    Some(UsageLine {
        uuid,
        msg_id: msg.get("id").and_then(|i| i.as_str()).map(str::to_string),
        at,
        tokens: token_sum(usage),
        cwd: v
            .get("cwd")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}

/// 四个桶平铺相加。**缺桶按 0 算**（实测四桶 100% 都在，但真出现缺桶时不该把整行丢掉）。
pub fn token_sum(usage: &Value) -> u64 {
    BUCKETS
        .iter()
        .filter_map(|k| usage.get(*k).and_then(|n| n.as_u64()))
        .sum()
}

/// epoch 秒 → **本地**日历日。
///
/// ⚠️ 转录里的时间戳是 **UTC**（`…Z`），而用户说的"今天"是**本地**的今天（UTC+8）。
/// 直接拿 UTC 的日子去比，每天 08:00 之前会把昨天的行算成今天（差 8 小时）。
pub fn local_date(at: i64) -> Option<chrono::NaiveDate> {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(at, 0)
        .single()
        .map(|t| t.date_naive())
}

/// 现在（本地）是几号。
pub fn today() -> chrono::NaiveDate {
    chrono::Local::now().date_naive()
}

/// token 数的显示格式：**K/M 缩写**（用户 2026-09-16 裁定）。
///
/// 规则（边界都钉在测试里）：
/// - `< 1000` 原样（`0` / `999`）—— 小数字加单位反而更难读；
/// - `1000 ≤ n` 用 `K`，保留**一位小数**（`1.0K` / `12.3K` / `980.0K`）；
/// - 四舍五入会撞到 `1000.0K` 时**进位到 M**（`999_999` → `1.0M`）——
///   否则会出现"1000.0K"这种既不是 K 也不是 M 的写法。
///
/// 刻意**不做**千位分隔（`165,007,052`）：挂件每行只有几十 pt，分隔符纯占地方。
pub fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let k = n as f64 / 1_000.0;
    if k < 999.95 {
        return format!("{k:.1}K");
    }
    format!("{:.1}M", n as f64 / 1_000_000.0)
}

/// 项目名 = `cwd` 的**末段**。
///
/// 用末段而不是全路径：挂件只有几行位置，全路径一行放不下；而末段在用户的目录习惯里
/// 就是项目名（`…\开发\claude-hud` → `claude-hud`）。
///
/// 两种分隔符都要认：真机 `cwd` 是 Windows 的反斜杠，但配置/测试里出现过正斜杠。
pub fn project_of(cwd: &str) -> String {
    // ⚠️ 先去掉尾部**所有**分隔符：`rsplit` 在 `D:\a\b\` 上会先切出一个空段
    // （实测踩过：尾斜杠的路径会变成"(未知)"）。
    let trimmed = cwd.trim_end_matches(['\\', '/']);
    let tail = trimmed.rsplit(['\\', '/']).next().unwrap_or(trimmed).trim();
    if tail.is_empty() {
        // 空 cwd（缺字段的行）——给一个显眼的名字，别让它悄悄并进某个真实项目里。
        "(未知)".to_string()
    } else {
        tail.to_string()
    }
}

/// 今日账本。
///
/// **只装当天的东西**：uuid 去重集合也只装当天的（今天实测约 1100 行，内存可忽略）。
/// 跨零点由 [`Ledger::reset_to`] 负责 —— 见那里的说明。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ledger {
    day: Option<chrono::NaiveDate>,
    /// 全机今日总量。
    pub total: u64,
    /// 项目名 → 今日量（`BTreeMap`：**确定性顺序**，方便断言与将来显示）。
    pub by_project: BTreeMap<String, u64>,
    /// 会话 id → 今日量。会话 id 取**转录文件名的主干**（`<session_id>.jsonl`）——
    /// 这是挂件里唯一能把"一份转录"与"一行会话"对上的键（状态文件里也有 `session_id`，
    /// 扫描器按文件名分组即可，不必再解析文件内容）。
    pub by_session: BTreeMap<String, u64>,
    /// 今天**已经采信的那一行**（键 = `message.id`，拿不到时用 `uuid`）。
    ///
    /// 为什么存整行而不是一个"见过"的集合：同一次响应会**陆续**写成多行，后写的行
    /// 可能带更完整的 `usage`（`output_tokens` 从 0 变成最终值）。所以要能在新行到来时
    /// **把旧的贡献减掉、换上新的** —— 只记"见过"就做不到。
    msgs: std::collections::HashMap<String, Msg>,
}

/// 账本里采信的**一次 API 调用**（组内时间戳最大的那一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Msg {
    at: i64,
    tokens: u64,
    project: String,
    session: String,
}

impl Ledger {
    /// 建一本"某一天"的账。
    pub fn for_day(day: chrono::NaiveDate) -> Self {
        Self {
            day: Some(day),
            ..Default::default()
        }
    }

    /// 这本账记的是哪一天（`None` = 还没定过，`Default` 出来的那种）。
    pub fn day(&self) -> Option<chrono::NaiveDate> {
        self.day
    }

    /// 跨零点：把账清零，改成新的一天。
    ///
    /// **必须连 `seen` 一起清** —— 不清的话，第二天出现的"昨天见过的 uuid"会被当成重复丢掉
    /// （而它其实是新一天里新写的一行）。
    pub fn reset_to(&mut self, day: chrono::NaiveDate) {
        *self = Self::for_day(day);
    }

    /// 记一行，返回**账本有没有因为这个而变**。
    ///
    /// 三件事按顺序做：
    /// 1. **不是今天的** → 丢掉（先判日期，"昨天的行"连键都不用建）；
    /// 2. **同一次响应已经有更晚（或同刻）的行** → 丢掉 —— 这正是"一次响应写三行"时
    ///    只算最后那一行的机制；
    /// 3. 否则：**先把旧那一行的贡献减掉**（如果是替换），再记新的。
    ///
    /// 第 3 步的减法是必需的：组内的行是**陆续**写进来的，后写的行带着更完整的
    /// `output_tokens`。只加不减会让"中间态 + 终态"一起进账（实测就是这条让总数虚高）。
    pub fn add(&mut self, session: &str, line: &UsageLine) -> bool {
        if local_date(line.at) != self.day {
            return false;
        }
        let key = line
            .msg_id
            .clone()
            .unwrap_or_else(|| line.uuid.clone());
        let project = project_of(&line.cwd);

        match self.msgs.get(&key) {
            // 已经有更晚（或同刻）的一行 ⇒ 这一行是中间态或重复，丢掉。
            Some(old) if old.at >= line.at => return false,
            // 这一行更晚 ⇒ 把旧的贡献减掉，随后连新的记上。
            Some(old) => {
                let (p, s, t) = (old.project.clone(), old.session.clone(), old.tokens);
                self.subtract(&p, &s, t);
            }
            None => {}
        }

        self.total = self.total.saturating_add(line.tokens);
        Self::bump(&mut self.by_project, &project, line.tokens);
        Self::bump(&mut self.by_session, session, line.tokens);
        self.msgs.insert(
            key,
            Msg {
                at: line.at,
                tokens: line.tokens,
                project,
                session: session.to_string(),
            },
        );
        true
    }

    /// 记上 `n`（`saturating`：账本里宁可少算一个 u64 溢出，也不 panic）。
    fn bump(map: &mut BTreeMap<String, u64>, key: &str, n: u64) {
        let e = map.entry(key.to_string()).or_insert(0);
        *e = e.saturating_add(n);
    }

    /// 把某一行先前记上的量**减掉**（替换时用）。减到 0 就把键删掉 ——
    /// 免得界面上出现一个"用量 0"的项目/会话条目。
    fn subtract(&mut self, project: &str, session: &str, n: u64) {
        self.total = self.total.saturating_sub(n);
        for (map, key) in [(&mut self.by_project, project), (&mut self.by_session, session)] {
            if let Some(v) = map.get_mut(key) {
                *v = v.saturating_sub(n);
                if *v == 0 {
                    map.remove(key);
                }
            }
        }
    }

    /// 给**只读消费者**（挂件 UI）的一份视图：`total` / `day` / `by_session`，
    /// 三样就是 `ui.rs` 真正读的全部（实测 `grep "ledger\." src/ui.rs`：只有
    /// `session_tokens(&row.session_id)` / `day()` / `total`）。
    ///
    /// ## 为什么要单独一个类型（而不再直接 `clone()` 整本账）
    ///
    /// 挂件**每帧**克隆一次快照（`ui.rs` 的 `App::ui`）。整本 `Ledger` 的克隆实测
    /// **172 µs**、只取这三样是 **1.8 µs**（release，2026-09-18 真机，同一套夹具同一时刻
    /// 对量，那是当天 34 个会话的账本），相差 **97 倍**；也就是说这 172 µs 里
    /// 几乎全部花在 UI **一个字都不读**的 `msgs` 上。
    /// 挂在桌面上的常驻件按秒重画，再加上拖动/悬停时按帧重画（60~144 fps），
    /// 这笔钱是纯浪费。
    ///
    /// ⚠️ **不是**"克隆时把 `msgs` 丢掉"就完事：`add` 靠 `msgs` 去重，一本 `msgs`
    /// 被掏空的账本**再加一次同一行就会把那一次调用算两遍**（转录里一次 API 响应写多行、
    /// 每行 uuid 都不同）—— 那是本项目最恨的"安静地算错"。所以**类型上就不给它这个可能**：
    /// `LedgerView` 根本没有 `add`。
    pub fn view(&self) -> LedgerView {
        LedgerView {
            day: self.day,
            total: self.total,
            by_session: self.by_session.clone(),
        }
    }
}

/// 只读账本视图 —— [`Ledger::view`] 的产物。字段与 [`Ledger`] 同名同义，
/// 唯独**没有 `msgs`**（那是扫描器的内部账，不外传；理由见 `Ledger::view`）。
///
/// 项目维度（`by_project`）刻意不在里面：界面上不显示它（用户 2026-09-16 裁定不做），
/// 需要它的只有 `--doctor`，而那边直接读真账本。将来若要在界面上按项目分组，
/// 往这里**显式**加一个字段即可 —— 别改成"顺手把整本账传出去"。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LedgerView {
    day: Option<chrono::NaiveDate>,
    /// 全机今日总量。
    pub total: u64,
    /// 会话 id → 今日量（与 [`Ledger::by_session`] 同一口径）。
    by_session: BTreeMap<String, u64>,
}

impl LedgerView {
    /// 这本视图记的是哪一天（`None` = 扫描器还没定过日）。
    pub fn day(&self) -> Option<chrono::NaiveDate> {
        self.day
    }

    /// 某条会话今天的量（没有记录就是 0）。会话行尾显示的就是它。
    pub fn session_tokens(&self, session: &str) -> u64 {
        self.by_session.get(session).copied().unwrap_or(0)
    }
}

// ---- T2：扫描器（唯一做 I/O 的部分）------------------------------------------

use std::path::{Path, PathBuf};

use crate::transcript;

/// 本地某天的**零点**（epoch 秒）。用于 `mtime` 预筛。
///
/// 东八区没有夏令时，但**不能假设**（用户换时区、或将来跑在别的机器上）：
/// `LocalResult` 不是 `Single` 时退化成 UTC 零点，宁可多扫几份，也不要因为 panic
/// 让整个挂件起不来。
pub fn day_start_epoch(day: chrono::NaiveDate) -> i64 {
    use chrono::{LocalResult, TimeZone};
    let naive = day
        .and_hms_opt(0, 0, 0)
        .expect("任何日期都有零点");
    match chrono::Local.from_local_datetime(&naive) {
        LocalResult::Single(t) => t.timestamp(),
        _ => naive.and_utc().timestamp(),
    }
}

/// 增量扫描 `~/.claude/projects/**` 里**今天动过**的转录，维护一本 [`Ledger`]。
///
/// ## 两条让它便宜下来的设计
///
/// 1. **`mtime` 预筛**：文件是**追加写**的，所以"今天没被写过的文件"**不可能含今天的行**。
///    实测这一条把启动扫描从 **163 份 / 325 MB** 砍到 **3 份 / 8.2 MB**（98%）。
///    ⚠️ 代价要说清：这条判据依赖"mtime 与内容一致"。**机器时间被改过**时它会漏——
///    所以配了一条防御性测试（造一份 mtime 是昨天、内容却是今天的行，断言它**不**被计入）。
/// 2. **按 offset 增量**：复用 `transcript::read_new`（只读新增字节、**半行不消费**），
///    与状态轮询走的是同一套纪律。长会话因此不随文件变大而变慢。
///
/// ## 跨零点
///
/// `scan` 每轮都拿当天的日期进来；一发现日期变了就 `reset_to` + **清空所有 offset**。
/// 清 offset 是必须的：丢弃的是"记在昨天账上的行"，但文件里还有**已经读过、却没被计入**
/// 的行（比如刚好跨过零点那几毫秒写的），得让它们在新的一天里重新被看见。
pub struct Scanner {
    root: PathBuf,
    offsets: std::collections::HashMap<PathBuf, u64>,
    ledger: Ledger,
}

impl Scanner {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            offsets: std::collections::HashMap::new(),
            ledger: Ledger::default(),
        }
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// 扫一轮，返回**这一轮新计入的行数**（测试与排查用）。
    pub fn scan(&mut self, day: chrono::NaiveDate) -> u32 {
        if self.ledger.day() != Some(day) {
            self.ledger.reset_to(day);
            self.offsets.clear();
        }
        let since = day_start_epoch(day);
        let mut counted = 0u32;
        for (path, session) in self.candidate_files(since) {
            let offset = self.offsets.get(&path).copied().unwrap_or(0);
            // 读不出来（文件刚好被删/被锁）就跳过这一份，不打断整轮 ——
            // 转录是别的进程在写，读失败是常态而不是异常。
            let Ok(Some((text, new_offset))) = transcript::read_new(&path, offset) else {
                continue;
            };
            for line in text.lines() {
                if let Some(u) = parse_usage_line(line)
                    && self.ledger.add(&session, &u)
                {
                    counted += 1;
                }
            }
            self.offsets.insert(path, new_offset);
        }
        counted
    }

    /// 今天动过的、该计入的转录文件：`(路径, 它属于哪条会话)`。
    ///
    /// 目录形状（真机实测）：
    /// ```text
    /// ~/.claude/projects/<编码后的项目目录>/<session-id>.jsonl              ← 主转录
    /// ~/.claude/projects/<编码后的项目目录>/<session-id>/subagents/agent-*.jsonl  ← 子代理
    /// ```
    ///
    /// **两层显式遍历，不做递归**：只认这两种形状，将来 Claude Code 换个目录布局会被
    /// 明确漏掉（而不是悄悄算进一堆不相干的东西）。
    ///
    /// ⚠️ **子代理的用量算进它的父会话**（路径里那层 `<session-id>` 目录），因为
    /// 子代理的 token 是**真花掉的**，而"会话行尾那个数"要能对上界面上的这一行 ——
    /// 若按文件名分组，它会变成一堆界面上不存在的 `agent-xxxx` 会话。
    fn candidate_files(&self, since: i64) -> Vec<(PathBuf, String)> {
        let mut out = Vec::new();
        let Ok(projects) = std::fs::read_dir(&self.root) else {
            return out; // 目录不存在：账本就是 0，不报错（与"没有会话"同一种安静）
        };
        for proj in projects.flatten() {
            let Ok(sub) = std::fs::read_dir(proj.path()) else {
                continue;
            };
            for entry in sub.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    // `<session-id>/subagents/*.jsonl`
                    let session = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
                    let subs = p.join("subagents");
                    if let Ok(files) = std::fs::read_dir(&subs) {
                        for f in files.flatten() {
                            let fp = f.path();
                            if is_jsonl(&fp) && modified_since(&fp, since) {
                                out.push((fp, session.to_string()));
                            }
                        }
                    }
                } else if is_jsonl(&p) && modified_since(&p, since) {
                    // 先取成 owned：`file_stem()` 借的是 `p`，而下面要把 `p` 移进结果里。
                    let session = p
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_string();
                    out.push((p, session));
                }
            }
        }
        out
    }
}

fn is_jsonl(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()) == Some("jsonl")
}

/// `mtime >= since`。读不到元数据时**返回 true**（宁可多扫一份，也不要因为权限问题漏掉）。
fn modified_since(p: &Path, since: i64) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return true;
    };
    let Ok(t) = meta.modified() else { return true };
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64 >= since,
        Err(_) => true, // 时间戳在 1970 之前：现实中不可能，同样按"多扫"处理
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一行真形态的转录（字段与真机一致，**只保留算账用得上的**）。
    ///
    /// ⚠️ **用序列化器造，不用 `format!` 拼**：`cwd` 是 Windows 路径，里面有反斜杠 ——
    /// 直接拼进 JSON 会造出**非法转义**（`"D:\公司..."`），`serde_json` 会整行拒收，
    /// 于是所有用例都会以"解析不出来"的形式红掉，而根因跟被测代码毫无关系。
    /// （实测踩过：第一版就是拼的，6 条用例全红。）
    fn line(uuid: &str, ts: &str, cwd: &str, buckets: [u64; 4]) -> String {
        line_with_msg(uuid, None, ts, cwd, buckets)
    }

    /// 同上，但可以指定 `message.id`（= **一次 API 响应的身份**，见模块头第 3 条）。
    fn line_with_msg(
        uuid: &str,
        msg_id: Option<&str>,
        ts: &str,
        cwd: &str,
        buckets: [u64; 4],
    ) -> String {
        let mut v = serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "uuid": uuid,
            "cwd": cwd,
            "message": { "usage": {
                "input_tokens": buckets[0],
                "output_tokens": buckets[1],
                "cache_read_input_tokens": buckets[2],
                "cache_creation_input_tokens": buckets[3],
                "service_tier": "standard"
            }}
        });
        if let Some(id) = msg_id {
            v["message"]["id"] = serde_json::Value::String(id.to_string());
        }
        v.to_string()
    }

    /// 本地（UTC+8）某天某时刻的 UTC 写法：`2026-09-16 00:00:00 +08:00` = `2026-09-15T16:00:00Z`。
    fn utc_at(local: &str) -> String {
        use chrono::TimeZone;
        let naive = chrono::NaiveDateTime::parse_from_str(local, "%Y-%m-%d %H:%M:%S").unwrap();
        let local_dt = chrono::Local
            .from_local_datetime(&naive)
            .single()
            .expect("本地时间必须唯一（本机无夏令时）");
        local_dt
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string()
    }

    #[test]
    fn four_buckets_are_summed_flat_and_missing_ones_count_as_zero() {
        let v: Value = serde_json::from_str(
            r#"{"input_tokens":1,"output_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}"#,
        )
        .unwrap();
        assert_eq!(token_sum(&v), 10);
        // 缺桶按 0 算（实测四桶 100% 都在，但真缺了不该把整行丢掉）
        let v: Value = serde_json::from_str(r#"{"input_tokens":7,"cache_read_input_tokens":3}"#).unwrap();
        assert_eq!(token_sum(&v), 10);
        // **没有 `usage` 里的四个键**（空对象）→ 0，而不是 panic
        assert_eq!(token_sum(&serde_json::from_str::<Value>("{}").unwrap()), 0);
    }

    #[test]
    fn only_assistant_lines_with_uuid_and_time_are_counted() {
        let good = line("u1", "2026-09-16T02:00:00.000Z", r"D:\projects\claude-hud", [1, 2, 3, 4]);
        assert_eq!(parse_usage_line(&good).unwrap().tokens, 10);

        // 不是 assistant 行（用户发言）—— 没有 usage，不该被当成 0 计入
        let user = r#"{"type":"user","timestamp":"2026-09-16T02:00:00.000Z","uuid":"u2","cwd":"x"}"#;
        assert!(parse_usage_line(user).is_none());
        // **没有 uuid**：算不了去重 ⇒ 丢掉（留着就是给总数注水）
        let no_uuid = r#"{"type":"assistant","timestamp":"2026-09-16T02:00:00.000Z","message":{"usage":{"input_tokens":5}}}"#;
        assert!(parse_usage_line(no_uuid).is_none());
        // **没有时间戳**：算不了"是不是今天" ⇒ 丢掉（宁少算，也别算到错误的日子里）
        let no_ts = r#"{"type":"assistant","uuid":"u3","message":{"usage":{"input_tokens":5}}}"#;
        assert!(parse_usage_line(no_ts).is_none());
        // 坏行不 panic
        assert!(parse_usage_line("{ 这不是 json").is_none());
    }

    #[test]
    fn the_day_boundary_is_local_midnight_not_utc() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        let mut l = Ledger::for_day(day);

        // 本地 09-16 00:00:00 整 —— **属于今天**（它的 UTC 写法是 09-15T16:00Z，
        // 直接拿 UTC 日子比会把它判成昨天：这就是那条 8 小时坑）
        let midnight = parse_usage_line(&line("a", &utc_at("2026-09-16 00:00:00"), r"D:\p\proj", [1, 0, 0, 0])).unwrap();
        assert_eq!(midnight.at % 86400, 57600, "前置条件：这条行确实是 UTC 的 16:00");
        assert!(l.add("s1", &midnight), "本地零点整必须算今天");

        // 本地 09-15 23:59:59 —— **不属于今天**
        let before = parse_usage_line(&line("b", &utc_at("2026-09-15 23:59:59"), r"D:\p\proj", [1, 0, 0, 0])).unwrap();
        assert!(!l.add("s1", &before), "昨天最后一秒不算今天");

        // 本地 09-16 23:59:59 —— 属于今天；再往后一秒就不是了
        let late = parse_usage_line(&line("c", &utc_at("2026-09-16 23:59:59"), r"D:\p\proj", [1, 0, 0, 0])).unwrap();
        assert!(l.add("s1", &late), "今天最后一秒算今天");
        let next = parse_usage_line(&line("d", &utc_at("2026-09-17 00:00:00"), r"D:\p\proj", [1, 0, 0, 0])).unwrap();
        assert!(!l.add("s1", &next), "次日零点整不算今天");

        assert_eq!(l.total, 2, "只该计入本地零点那条与末尾那条");
    }

    /// **夹具要覆盖真机上真实出现过的每一种形态**（形态清单来自 2026-09-16 的全机普查：
    /// 42,710 行 assistant + 22 万行级转录）。
    ///
    /// ⚠️ **这些"形态"取自真机，但内容是构造的**：真转录行属于敏感数据，**不入库**
    /// （本仓 1.0 要发布）。所以下面钉的是"每种形态我们算得对不对"，而不是"某一行原文"。
    ///
    /// 形态清单（每条都对应真机上的一个真实占比）：
    /// 1. **只有 `input`/`output`**，**没有 cache 那两个键**（全机 **12.9%** —— 历史版本）；
    /// 2. `message.id` 带 **`msg_` 前缀**（与裸 UUID 两种写法都真实存在）；
    /// 3. **四桶全 0**（真机存在这种行：整个响应没有任何 token 计入）；
    /// 4. **同一次响应的多行**（thinking/text/tool_use），`output` 从 0 递增到终值；
    /// 5. **同一 `message.id` 的重复行**（跨文件复制）；
    /// 6. `cache_creation` 远大于 `input`（真机上见过 30 万量级的一次缓存写入）。
    #[test]
    fn fixtures_cover_the_shapes_that_actually_occur_on_this_machine() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();

        // ① 缺 cache 桶：按 0 算，**不许整行丢掉**（丢掉的话这 12.9% 的量就凭空消失）
        let raw = r#"{"type":"assistant","timestamp":"2026-09-16T02:00:00.000Z","uuid":"u-nocache","cwd":"D:/p/x","message":{"id":"m-nocache","usage":{"input_tokens":41468,"output_tokens":0}}}"#;
        let l = parse_usage_line(raw).expect("缺桶不等于坏行");
        assert_eq!(l.tokens, 41468, "只有 input+output 时，和就是这两项");
        assert_eq!(l.msg_id.as_deref(), Some("m-nocache"));

        // ② `msg_` 前缀的 `message.id` 照样当键用
        let raw = r#"{"type":"assistant","timestamp":"2026-09-16T02:00:00.000Z","uuid":"u-pre","cwd":"D:/p/x","message":{"id":"msg_202609011650276a","usage":{"input_tokens":485,"output_tokens":163,"cache_read_input_tokens":385920,"cache_creation_input_tokens":0}}}"#;
        assert_eq!(parse_usage_line(raw).unwrap().tokens, 485 + 163 + 385_920);

        // ③ 四桶全 0：是一行**真实存在**的数据，不该被当成坏行丢掉
        let raw = r#"{"type":"assistant","timestamp":"2026-09-16T02:00:00.000Z","uuid":"u-zero","cwd":"D:/p/x","message":{"id":"m-zero","usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
        let z = parse_usage_line(raw).expect("全 0 也是合法行");
        assert_eq!(z.tokens, 0);
        let mut led = Ledger::for_day(day);
        assert!(led.add("s", &z), "全 0 的行也要入账（否则同组后续的终值行会被当成孤立行）");

        // ④ `cache_creation` 可以远大于 `input`（真机见过 304,310 / 6）
        let raw = r#"{"type":"assistant","timestamp":"2026-09-16T02:00:00.000Z","uuid":"u-cc","cwd":"D:/p/x","message":{"id":"m-cc","usage":{"input_tokens":6,"output_tokens":228,"cache_read_input_tokens":0,"cache_creation_input_tokens":304310}}}"#;
        assert_eq!(parse_usage_line(raw).unwrap().tokens, 6 + 228 + 304_310);

        // ⑤ 同一 `message.id` 的重复行（真机 2108 行重复，全是跨文件）只算一次 ——
        //    与 `the_same_uuid_in_two_files_counts_once` 的区别：这里连 uuid 都一样。
        let mut led = Ledger::for_day(day);
        let raw = r#"{"type":"assistant","timestamp":"2026-09-16T02:00:00.000Z","uuid":"u-dup","cwd":"D:/p/x","message":{"id":"m-dup","usage":{"input_tokens":376,"output_tokens":819,"cache_read_input_tokens":319744,"cache_creation_input_tokens":0}}}"#;
        let a = parse_usage_line(raw).unwrap();
        assert!(led.add("s", &a));
        assert!(!led.add("s", &a), "同一行再来一次必须丢掉");
        assert_eq!(led.total, 376 + 819 + 319_744);
    }

    /// ⭐ **一次 API 响应写成多行时只算一次，且算它的最终值。**
    ///
    /// 这是本模块最容易算错的一条（第一版按行累加，实测**虚高 2.29 倍**）。
    /// 三条行的数字**逐字取自真机**（`6f3b8d18…/subagents/agent-3d5f9b2c7e814a06.jsonl`
    /// 第 3~7 行）：thinking / text 两行写着 `output_tokens: 0`（那一刻还没生成完），
    /// 最后一行才是 183。
    #[test]
    fn one_response_written_as_three_lines_counts_once_at_its_final_value() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        let mut l = Ledger::for_day(day);
        let mid = "a928250d-9908-44a7-ba6c-94fde63a8ebc";
        let rows = [
            ("u1", "2026-09-16T02:00:00.000Z", [1045u64, 0, 51072, 0]), // thinking
            ("u2", "2026-09-16T02:00:01.000Z", [1045, 0, 51072, 0]),    // text
            ("u3", "2026-09-16T02:00:02.000Z", [1045, 183, 51072, 0]),  // tool_use：终值
        ];
        for (uuid, ts, buckets) in rows {
            let raw = line_with_msg(uuid, Some(mid), &utc_at(&ts.replace('T', " ").replace(".000Z", "")), r"D:\p\proj", buckets);
            let ul = parse_usage_line(&raw).expect("夹具必须是合法 JSON 行");
            assert_eq!(ul.msg_id.as_deref(), Some(mid), "夹具必须带上 message.id");
            l.add("s1", &ul);
        }
        assert_eq!(
            l.total,
            1045 + 183 + 51072,
            "只该算**一次**，而且是最终那一行的值（按行累加会是 {}）",
            1045 + 0 + 51072 + (1045 + 0 + 51072) + (1045 + 183 + 51072)
        );
        assert_eq!(l.view().session_tokens("s1"), 52300);
        assert_eq!(l.by_project["proj"], 52300);
    }

    /// **反向对照**：`message.id` 不同就是**两次**调用（哪怕它在同一毫秒、同一次请求链里），
    /// 不能被当成同一次而吃掉一笔。少了这条，把"按 message.id 分组"写成"每个会话只算一行"
    /// 这种离谱实现也能过。
    #[test]
    fn different_message_ids_are_different_calls() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        let mut l = Ledger::for_day(day);
        for (uuid, mid) in [("u1", "msg-a"), ("u2", "msg-b")] {
            let raw = line_with_msg(uuid, Some(mid), &utc_at("2026-09-16 10:00:00"), r"D:\p\proj", [100, 0, 0, 0]);
            assert!(l.add("s1", &parse_usage_line(&raw).unwrap()));
        }
        assert_eq!(l.total, 200, "两个 message.id = 两次调用，各算一笔");
    }

    /// 组内**先到的那一行更新**时，旧贡献必须被减掉 —— 否则"中间态 + 终态"一起进账，
    /// 正是虚高 2.29 倍的那个机制。
    #[test]
    fn replacing_a_earlier_line_subtracts_its_contribution() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        let mut l = Ledger::for_day(day);
        let mk = |uuid: &str, ts: &str, out: u64| {
            let raw = line_with_msg(uuid, Some("msg-x"), ts, r"D:\p\proj", [1000, out, 0, 0]);
            parse_usage_line(&raw).unwrap()
        };
        assert!(l.add("s1", &mk("u1", &utc_at("2026-09-16 10:00:00"), 0)));
        assert_eq!(l.total, 1000, "中间态先入账");
        assert!(l.add("s1", &mk("u2", &utc_at("2026-09-16 10:00:05"), 500)), "更晚的一行要替换它");
        assert_eq!(l.total, 1500, "替换而不是叠加（叠加会是 2500）");
        assert_eq!(l.view().session_tokens("s1"), 1500);
        // 同刻（不更晚）的那一行不许替换
        assert!(!l.add("s1", &mk("u3", &utc_at("2026-09-16 10:00:05"), 9999)));
        assert_eq!(l.total, 1500, "同刻的行不该动账本");
    }

    /// **没有 `message.id` 的行退回用 `uuid` 当键** —— 缺字段不等于可以把这行算两遍。
    #[test]
    fn a_line_without_message_id_falls_back_to_its_uuid() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        let mut l = Ledger::for_day(day);
        let raw = line("u1", &utc_at("2026-09-16 10:00:00"), r"D:\p\proj", [7, 0, 0, 0]);
        let ul = parse_usage_line(&raw).unwrap();
        assert!(ul.msg_id.is_none(), "这份夹具本来就没有 message.id");
        assert!(l.add("s1", &ul));
        assert!(!l.add("s1", &ul), "同一个 uuid 再来一次必须丢掉");
        assert_eq!(l.total, 7);
    }

    #[test]
    fn the_same_uuid_in_two_files_counts_once() {
        // ⭐ 实测：全机 2108 行重复 uuid，**全部是跨文件**、文件内 0 例（会话被 resume/复制）。
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        let mut l = Ledger::for_day(day);
        let a = parse_usage_line(&line("same", &utc_at("2026-09-16 10:00:00"), r"D:\p\one", [100, 0, 0, 0])).unwrap();
        // 同一 uuid、不同文件、不同项目名 —— 若不去重，总数会虚高、且会往两个项目里各记一笔
        let b = parse_usage_line(&line("same", &utc_at("2026-09-16 10:00:00"), r"D:\p\two", [100, 0, 0, 0])).unwrap();

        assert!(l.add("s1", &a), "第一次见到这个 uuid，计入");
        assert!(!l.add("s2", &b), "第二次见到同一个 uuid，必须丢掉");
        assert_eq!(l.total, 100, "重复行不许把总数抬成 200");
        assert_eq!(
            l.by_project.keys().collect::<Vec<_>>(),
            vec!["one"],
            "重复行也不许往第二个项目里记一笔"
        );
    }

    #[test]
    fn projects_are_taken_per_line_not_per_session() {
        // ⭐ 实测 79/163 份转录含多个 cwd ⇒ 项目归属只能按行取。
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        let mut l = Ledger::for_day(day);
        for (i, (cwd, n)) in [
            (r"D:\projects\样例看板", 10u64),
            (r"D:\projects\样例系统", 20),
            (r"D:\projects\样例看板", 5),
        ]
        .into_iter()
        .enumerate()
        {
            let raw = line(
                &format!("u{i}"),
                &utc_at("2026-09-16 10:00:00"),
                cwd,
                [n, 0, 0, 0],
            );
            assert!(l.add("s1", &parse_usage_line(&raw).unwrap()));
        }
        assert_eq!(l.total, 35);
        assert_eq!(l.by_project["样例看板"], 15, "同一项目的多行要累加");
        assert_eq!(l.by_project["样例系统"], 20);
    }

    #[test]
    fn crossing_midnight_resets_everything_including_the_seen_set() {
        let d16 = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        let d17 = chrono::NaiveDate::from_ymd_opt(2026, 9, 17).unwrap();
        let mut l = Ledger::for_day(d16);
        let a = parse_usage_line(&line("u1", &utc_at("2026-09-16 10:00:00"), r"D:\p\proj", [9, 0, 0, 0])).unwrap();
        assert!(l.add("s1", &a));
        assert_eq!(l.total, 9);

        l.reset_to(d17);
        assert_eq!(l.total, 0, "跨零点后总量必须归零");
        assert!(l.by_project.is_empty());

        // ⚠️ `seen` 也要清：否则"第二天又见到同一个 uuid"会被当成重复丢掉
        let again = parse_usage_line(&line("u1", &utc_at("2026-09-17 10:00:00"), r"D:\p\proj", [7, 0, 0, 0])).unwrap();
        assert!(l.add("s1", &again), "新的一天里同一个 uuid 应当重新计入");
        assert_eq!(l.total, 7);
    }

    #[test]
    fn each_session_keeps_its_own_todays_number() {
        // 会话行尾显示的就是这个数（用户 2026-09-16 裁定；为什么不是"所属项目"见模块头）。
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
        let mut l = Ledger::for_day(day);
        let a = parse_usage_line(&line("u1", &utc_at("2026-09-16 10:00:00"), r"D:\p\proj", [10, 0, 0, 0])).unwrap();
        let b = parse_usage_line(&line("u2", &utc_at("2026-09-16 11:00:00"), r"D:\p\proj", [5, 0, 0, 0])).unwrap();
        let c = parse_usage_line(&line("u3", &utc_at("2026-09-16 11:30:00"), r"D:\p\other", [7, 0, 0, 0])).unwrap();
        assert!(l.add("sess-a", &a));
        assert!(l.add("sess-b", &b));
        assert!(l.add("sess-b", &c));

        assert_eq!(l.view().session_tokens("sess-a"), 10);
        assert_eq!(l.view().session_tokens("sess-b"), 12, "同一会话的多行要累加");
        assert_eq!(l.view().session_tokens("没有这个会话"), 0, "没记录就是 0，不是 panic");
        assert_eq!(l.total, 22);
        // 行内之和 ≤ 总计：文件路径拿不到、或会话已不在列表里的量仍然算进总计 —— **这是设计**
        assert_eq!(l.by_project["proj"], 15, "项目维度按行取 cwd，与会话维度互不影响");
    }

    /// ⭐ **`view()` 必须逐项给出 UI 真正读的那三样**（`day` / `total` / 每会话的量）。
    ///
    /// 它是界面上那两个数字的**唯一来源**，少带一项不是报错、是界面上安静地少一个数。
    /// 反向对照：这条用例里的两个数**故意不相等**（总量含一条不在列表里的会话），
    /// 于是"把 `total` 当成 `session_tokens` 返回"这种实现也会红。
    #[test]
    fn the_view_carries_exactly_the_numbers_the_ui_reads() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 18).unwrap();
        let mut l = Ledger::for_day(day);
        let a = parse_usage_line(&line("u1", &utc_at("2026-09-18 10:00:00"), r"D:\p\proj", [10, 0, 0, 0])).unwrap();
        let gone = parse_usage_line(&line("u2", &utc_at("2026-09-18 11:00:00"), r"D:\p\proj", [93, 0, 0, 0])).unwrap();
        assert!(l.add("s1", &a));
        assert!(l.add("已关闭的会话", &gone));

        let v = l.view();
        assert_eq!(v.day(), Some(day), "视图必须带上「这是哪一天的账」");
        assert_eq!(v.total, 103, "总量含已关闭会话");
        assert_eq!(v.session_tokens("s1"), 10, "行尾那个数是这条会话自己的，不是总量");
        assert_eq!(v.session_tokens("没有这个会话"), 0, "没记录就是 0，不是 panic");
        assert_eq!(v.total, l.total, "视图与真账本必须逐项相同");
        assert_eq!(v.session_tokens("s1"), l.view().session_tokens("s1"));
    }

    /// ⭐ 视图是**值快照**，不是活引用：账本事后的变动不许漏进已经交给 UI 的那一份。
    ///
    /// 直接后果是"界面上的数会自己跳"——挂件每帧读的是同一份快照，读到一半被后台线程
    /// 改了，就会出现"同一个画面里顶栏和行尾口径不一致"这种没法复现的错。
    ///
    /// 夹具用的是**同一次响应的后续行**（同一 `message.id`、`output` 从 0 变成终值）——
    /// 真机上最常见的替换形态，`Ledger::add` 会先减旧的再加新的。
    #[test]
    fn a_view_already_handed_out_does_not_change_when_the_ledger_does() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 18).unwrap();
        let mut l = Ledger::for_day(day);
        let first = parse_usage_line(&line_with_msg(
            "u1", Some("msg-x"), &utc_at("2026-09-18 10:00:00"), r"D:\p\proj", [1000, 0, 0, 0],
        ))
        .unwrap();
        assert!(l.add("s1", &first));

        let v = l.view();
        assert_eq!(v.session_tokens("s1"), 1000);

        // 同一次响应的终值行到达：账本变成 1500，**已交出去的视图必须还是 1000**
        let later = parse_usage_line(&line_with_msg(
            "u2", Some("msg-x"), &utc_at("2026-09-18 10:00:05"), r"D:\p\proj", [1000, 500, 0, 0],
        ))
        .unwrap();
        assert!(l.add("s1", &later), "更晚的一行必须替换掉旧的那一行");
        assert_eq!(l.total, 1500, "真账本跟着变");
        assert_eq!(v.total, 1000, "已交出去的视图**不许**跟着变（它是快照）");
        assert_eq!(v.session_tokens("s1"), 1000);
    }

    // ---- T2：扫描器（真文件、真目录）-----------------------------------------

    use crate::testtmp::TempDir;

    /// 造一棵 `projects/<proj>/<session>.jsonl` 的目录树，返回根目录。
    fn tree(tag: &str, files: &[(&str, &str)]) -> TempDir {
        let root = TempDir::new("usage", tag);
        for (rel, content) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            // ⚠️ **行尾必须有换行**：增量读取器只消费到最后一个换行符为止（半行不记账），
            // 真转录的最后一个字节就是换行。不写换行的话，夹具里的行会被整段忽略 ——
            // 而症状是"扫描器一行都没读到"，看着像扫描器坏了。
            // （这条坑本仓已经踩过一次，记在 `docs/工作日志.md` 里。）
            std::fs::write(&path, format!("{content}\n")).unwrap();
        }
        root
    }

    /// 把一份文件的 mtime 拨到 `epoch` 秒。**`mtime` 预筛那条判据只能这么做才能测到。**
    fn set_mtime(p: &Path, epoch: i64) {
        let f = std::fs::File::options().write(true).open(p).unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(epoch as u64);
        f.set_modified(t).unwrap();
    }

    fn d16() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap()
    }

    #[test]
    fn the_scanner_counts_todays_lines_and_attributes_subagents_to_their_parent_session() {
        let today = d16();
        let main = line("u1", &utc_at("2026-09-16 10:00:00"), r"D:\p\样例看板", [10, 0, 0, 0]);
        let sub = line("u2", &utc_at("2026-09-16 10:05:00"), r"D:\p\样例看板", [5, 0, 0, 0]);
        let root = tree(
            "scan",
            &[
                ("D-------claude/sess-aaa.jsonl", &main),
                ("D-------claude/sess-aaa/subagents/agent-x.jsonl", &sub),
            ],
        );

        let mut sc = Scanner::new(&*root);
        assert_eq!(sc.scan(today), 2, "两份转录各计入一行");
        assert_eq!(sc.ledger().total, 15);
        assert_eq!(sc.ledger().by_project["样例看板"], 15);
        assert_eq!(
            sc.ledger().view().session_tokens("sess-aaa"),
            15,
            "子代理的用量要算进它的父会话（否则界面上会多出 agent-x 这种不存在的会话）"
        );
        assert_eq!(sc.ledger().view().session_tokens("agent-x"), 0);
    }

    #[test]
    fn the_scanner_is_incremental_and_never_double_counts() {
        let today = d16();
        let root = tree(
            "incr",
            &[(
                "proj/s1.jsonl",
                &line("u1", &utc_at("2026-09-16 10:00:00"), r"D:\p\a", [1, 0, 0, 0]),
            )],
        );
        let file = root.join("proj/s1.jsonl");
        let mut sc = Scanner::new(&*root);

        assert_eq!(sc.scan(today), 1);
        assert_eq!(sc.ledger().total, 1);
        assert_eq!(sc.scan(today), 0, "没有新字节时不该再计入任何东西");
        assert_eq!(sc.ledger().total, 1, "重复扫不能把总数翻倍");

        // 追加一整行 → 只多算这一行
        let mut f = std::fs::File::options().append(true).open(&file).unwrap();
        use std::io::Write;
        writeln!(f, "{}", line("u2", &utc_at("2026-09-16 11:00:00"), r"D:\p\a", [2, 0, 0, 0])).unwrap();
        drop(f);
        assert_eq!(sc.scan(today), 1);
        assert_eq!(sc.ledger().total, 3);

        // 追加**半行**（没有换行）→ 不该被计入，补齐后才算 —— 与状态轮询同一条纪律
        let mut f = std::fs::File::options().append(true).open(&file).unwrap();
        write!(f, "{}", line("u3", &utc_at("2026-09-16 12:00:00"), r"D:\p\a", [4, 0, 0, 0])).unwrap();
        drop(f);
        assert_eq!(sc.scan(today), 0, "半行不消费（否则 JSON 被从中间截断，那条永久丢失）");
        assert_eq!(sc.ledger().total, 3);
        let mut f = std::fs::File::options().append(true).open(&file).unwrap();
        writeln!(f).unwrap();
        drop(f);
        assert_eq!(sc.scan(today), 1, "补齐换行之后就该算上");
        assert_eq!(sc.ledger().total, 7);
    }

    #[test]
    fn a_file_not_modified_today_is_not_scanned_even_if_it_contains_todays_lines() {
        // ⭐ 这条是 `mtime` 预筛的**防御性**测试：预筛假定"没被写过 ⇒ 不含今天的行"，
        // 而那依赖"mtime 与内容一致"。机器时间被改过时该假设不成立 —— 本用例就是那个反例，
        // 断言此时**宁可漏**（而不是把整机 325MB 全读一遍）。判据写进注释，免得将来有人
        // 把预筛当成"绝对正确"的优化而删掉这条测试。
        let today = d16();
        let yesterday_mtime = day_start_epoch(today) - 3600; // 今天零点前 1 小时
        let root = tree(
            "mtime",
            &[
                (
                    "proj/fresh.jsonl",
                    &line("u1", &utc_at("2026-09-16 10:00:00"), r"D:\p\a", [1, 0, 0, 0]),
                ),
                (
                    "proj/stale.jsonl",
                    &line("u2", &utc_at("2026-09-16 10:00:00"), r"D:\p\b", [100, 0, 0, 0]),
                ),
            ],
        );
        set_mtime(&root.join("proj/stale.jsonl"), yesterday_mtime);

        let mut sc = Scanner::new(&*root);
        sc.scan(today);
        assert_eq!(sc.ledger().view().session_tokens("fresh"), 1, "今天动过的照常计入");
        assert_eq!(
            sc.ledger().view().session_tokens("stale"),
            0,
            "mtime 是昨天的一律跳过 —— 这是预筛的代价，明写在注释里"
        );
    }

    #[test]
    fn crossing_midnight_makes_the_scanner_re_read_from_the_start() {
        // ⚠️ 这里用**昨天 → 今天**（而不是两个虚构的日期）：`mtime` 预筛拿的是**今天零点**，
        // 而文件的 mtime 是"现在"—— 用未来的日期当日界，文件会被预筛直接跳过，
        // 症状是"跨零点后什么都没读到"，看着像 offset 没清。**（第一版就踩了这个。）**
        let d_yesterday = chrono::NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let d_today = d16();
        let root = tree(
            "midnight",
            &[(
                "proj/s1.jsonl",
                &format!(
                    "{}\n{}",
                    line("u1", &utc_at("2026-09-15 23:59:00"), r"D:\p\a", [9, 0, 0, 0]),
                    line("u2", &utc_at("2026-09-16 00:01:00"), r"D:\p\a", [3, 0, 0, 0])
                ),
            )],
        );
        let mut sc = Scanner::new(&*root);

        // 昨天那一轮：**两行都会被读到**（同一个文件），但只有 09-15 那行算进昨天的账。
        assert_eq!(sc.scan(d_yesterday), 1);
        assert_eq!(sc.ledger().total, 9);
        assert_eq!(sc.ledger().day(), Some(d_yesterday));

        // 跨零点：账本清零、**offset 也清掉** —— 于是那行"已经读过、却被日期挡掉"的
        // 新一天的行会被重新读到并计入。若只清账本不清 offset，这里就会是 0。
        assert_eq!(sc.scan(d_today), 1, "跨零点必须重新从头读");
        assert_eq!(sc.ledger().total, 3, "新账本里只有新一天的那一行");
        assert_eq!(sc.ledger().day(), Some(d_today));
        assert_eq!(sc.scan(d_today), 0, "再扫一轮不该重复计入");
    }

    #[test]
    fn token_numbers_are_abbreviated_with_k_and_m() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(1), "1");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1.0K");
        assert_eq!(fmt_tokens(12_345), "12.3K");
        assert_eq!(fmt_tokens(980_000), "980.0K");
        // 边界：**四舍五入会撞到 1000.0K 时进位到 M**，而不是写出"1000.0K"
        assert_eq!(fmt_tokens(999_949), "999.9K");
        assert_eq!(fmt_tokens(999_999), "1.0M");
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
        assert_eq!(fmt_tokens(165_007_052), "165.0M");
        // 再大也不会溢出成科学计数法
        assert_eq!(fmt_tokens(u64::MAX), "18446744073709.6M");
    }

    #[test]
    fn a_missing_cwd_becomes_a_visible_placeholder_not_a_silent_merge() {
        assert_eq!(project_of(r"D:\projects\claude-hud"), "claude-hud");
        assert_eq!(project_of("D:/projects/web-lab"), "web-lab", "正斜杠也要认");
        assert_eq!(project_of(r"D:\one\two\"), "two", "尾部分隔符不该产生空名");
        assert_eq!(project_of(""), "(未知)", "缺 cwd 的行不能被悄悄并进某个真实项目");
    }
}
