use crate::config;
use crate::cursor;
use crate::model::State;
use crate::paths;
use crate::poller;
use crate::procinfo;
use crate::state;
use crate::transcript;
use crate::usage;
use crate::view;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 后台轮询线程写入、渲染线程每帧读取的快照。
///
/// 轮询**不在渲染线程**：首次轮询要读完整份 transcript（本机实测 5 个大会话共
/// 88 MB → 730 ms），放在帧里会把它变成一次可感的卡顿。
type Snapshot = Arc<Mutex<SnapshotData>>;

/// 快照里装的东西：这一轮的行 + 这一轮的今日 token 账本。
///
/// 账本与行**同一轮产出、同一把锁**：它们来自同一次后台扫描，分开放会出现
/// "行是这一轮的、数字是上一轮的"这种只有肉眼能看出来的错位。
#[derive(Default, Clone)]
struct SnapshotData {
    rows: Vec<view::Row>,
    ledger: usage::LedgerView,
    /// **第一次扫描是否已经落地。**
    ///
    /// 首轮要读完整份 transcript（本机实测 5 个大会话 88 MB → 730 ms），所以窗口开出来
    /// 的头一秒里快照还是 `Default` —— `rows` 空、`ledger` 空。**"空"有两种含义**：
    /// "扫过了，确实没有活跃会话"与"还没扫"。界面上原先把前者说了出来，于是每次启动都先
    /// 亮一句"● claude 会话 · **0 个** / 没有活跃会话"再改成真话（遗留 #6）。
    ///
    /// 这一位就是那个区分。与顶栏对 `ledger.day()` 的既有口径**同一条规矩**：
    /// **读不到 ≠ 0，读不到就少说一句，不编一个数出来。**
    first_scan_done: bool,
}

// ---- 外观常量（用户 2026-09-14 裁定："UI 改成小游戏风格，白底黑边，数码字体"）----

/// 面板填充：**纯白、不透明**。用户上一版抱怨"背景和文字颜色一个颜色"（深灰底 +
/// 浅灰字），白底 + 黑字是这条抱怨的直接对策。
const PANEL_FILL: egui::Color32 = egui::Color32::WHITE;
/// 内容与面板边缘之间的留白。
///
/// ⚠️ 这里原本还挨着一个 `BORDER_WIDTH = 3.0`（粗黑描边）—— **2026-09-16 用户裁掉**：
/// "现在有一点点黑色的边框，**我希望去掉，纯白色**"。黑边是 2026-09-14 的"小游戏 HUD 框感"
/// 要求的，用户看过实机之后推翻了它。**这个常量连同描边一起删掉**，
/// 留白本身保留（去掉它文字会贴到窗口边上）。
///
/// 连带效果：内容区左右各让出 3pt（`inner_right` 不再扣描边），
/// 最小宽度那张退化表因此重测过。
const INNER_MARGIN: i8 = 8;
/// 白底上的正文颜色。用户点名要黑色（"背景和文字颜色一个颜色"的反面）。
const TEXT_COLOR: egui::Color32 = egui::Color32::BLACK;
/// 会名 / 状态词与计时 / 顶栏的字号。用户裁定：会名 17、状态 15（原为 13 / 12）。
const NAME_SIZE: f32 = 17.0;
const STATE_SIZE: f32 = 15.0;
const HEADER_SIZE: f32 = 13.0;

/// **第二行**（`上下文 N%` + 今日 token + `子代理 N`）的字号。
///
/// 两笔用户裁定叠起来：2026-09-15"**缩小 50%**"（= 状态词的一半），
/// 2026-09-22"**放大 1.5 倍**"（= 半个状态词再乘 1.5 = 状态词的 0.75）。
///
/// （旧注释叫它"第三行" —— 那是每会话**三行**那个版本的叫法，三行布局已被用户否掉，
/// 现在一行会话只有两行。同一个词残留在别处会让人以为界面上还有第三行。）
///
/// 写成 `STATE_SIZE * 0.75` 而不是 `11.25`：这个值**定义为状态词的一个比例**，
/// 状态词将来若改，它跟着走；写死数字会让两者悄悄脱钩。
///
/// ⚠️ 这一行**撑起整个会话块的高度**（见 `draw_row` 的 `ctx_row_h`）：字号一动，
/// 每条会话占的纵向空间就跟着动，默认窗口能装几条也会变 —— 别以为只是"字大了一点"。
const CONTEXT_SIZE: f32 = STATE_SIZE * 0.75;

/// 会话之间**额外**补的空白。用户 2026-09-15 裁定："每个会话的间距**扩大一倍**"。
///
/// ⚠️ 历史数字别再用：`29pt`（"第三行 → 下一行会名"）与 `52pt`（会名→会名）是
/// **每会话三行**那个版本（`881e951`）量的，而三行布局第二天就被用户否掉了。
/// 到本轮的基准版（`a407ba7`，两行布局）同一对量是 **58 / 88**（2026-09-16 复审实测）。
/// 也就是说：**"翻倍即 58"这句话只在三行时代成立**，两行时代是"补到 58"。
///
/// 见 `rows_are_spaced_twice_as_far_apart_as_before` —— 那个测试钉的是**最终效果 58**
/// （galley 原点差；同一处的视觉空隙是 46 = 58 − 第二行自己的 galley 高 12），将来谁动了
/// 行高、`rule()` 或 `item_spacing` 都会让它失败，而不是界面悄悄变回去。
///
/// ⚠️ **这个值随另外三个间距常量与行高联动。** 第三轮（行高显式钉死 + `item_spacing.y`
/// 清零 + 分隔线零占位 + `ROW_LINE_GAP`/`ROW_BOTTOM_GAP` 改写）把这条量到的间隔**整体压掉了
/// 9pt** —— 33 只能量到 49。故按实测补到 42，才回到 58。
/// （补偿量是**量出来的**，不是推出来的：`cargo test rows_are_spaced` 会把实测值打在
/// 失败信息里，"33 → 30"那版推算是错的，作废。）
///
/// 2026-09-22 又补了一次，**同一个手法、同一个理由**：第二行字号 7.5 → 11.25pt
/// ⇒ 那一行的 galley 高 8 → 12 ⇒ 会话块高 44 → 48、实测间距 58 → **62**，
/// 而默认窗 320×380 因此只装得下 3 条会话（`four_sessions_all_fit_at_the_default_size`
/// 立刻红 —— 用户 2026-09-17 专门报过"四个会话为什么只显示了 3 个"）。
/// 用户裁定**补这一头**：42 → **38**，于是 58 与"默认窗装 4 条"都回来，
/// 代价是会话之间**视觉空隙 50 → 46**（那一处的线上下不对称是刻意的：线属于上面那条会话）。
/// ⚠️ 别把 38 当成"审美值"去微调 —— 它是"让量到的效果回到裁定的 58"反解出来的。
/// 只要看见这条测试红，先想"是不是动了行高或另外三处间距"，别直接改这里的数字。
const EXTRA_ROW_GAP: f32 = 38.0;

/// 第一行 → 第二行（`上下文 N%`）的空隙 = **界面上量到的那个数**。
/// 用户 2026-09-15 第三轮裁定 **14pt**。
///
/// 这一条修的是**归属感**：改前是 8pt，而第二行 → 分隔线是 11pt —— 第二行离
/// **下面的线**比离**它注释的那一行**还远，读起来像掉队的注脚。
/// （注意 origin 间距给人的印象是反的：改前 origin 差 27pt，看起来很健康。
/// 差别全在第二行只有 8pt 高。）
const ROW_LINE_GAP: f32 = 14.0;

/// 第二行 → 分隔线的空隙 = **界面上量到的那个数**。
/// 用户 2026-09-15 第三轮裁定 **8pt**（改前 11pt）。
///
/// 与 [`ROW_LINE_GAP`] 配对：上大下小，第二行才明确"属于上面那一行"。
const ROW_BOTTOM_GAP: f32 = 8.0;

/// 分隔线 → 第一条会话的空隙 = **界面上量到的那个数**。
/// 用户 2026-09-15 第三轮裁定 **16pt**（改前 6pt）。
///
/// 改前顶栏到第一条会话只有 6pt，**比会话内部两行之间的 8pt 还紧** —— 顶栏（次要
/// 信息）与正文（主要信息）之间的空白比正文内部还小，层级是倒的。
const HEADER_GAP: f32 = 16.0;

/// 分隔线的线宽。与 egui 主题里那条 `1.0` 同值 —— 换掉画法前后像素不变。
const RULE_WIDTH: f32 = 1.0;

/// 分隔线（顶栏下一条、每两条会话之间一条）的颜色。**显式常量，不取主题。**
///
/// 2026-09-21 定：用户看过候选后裁定**保持现状的 `#BEBEBE`**（纯白底上对比 1.85:1，
/// 很淡 —— 与"线只出现在新模块开始处、模块内部不画线"的偏好一致）。
///
/// 为什么必须钉成常量：`egui::Separator` 的描边取自 **egui 主题**
/// （`visuals.noninteractive.bg_stroke`），浅色主题是 `gray(190)` = `#BEBEBE`、
/// **深色主题是 `gray(60)` = `#3C3C3C`**（画在纯白底上反而更重）。于是**同一个二进制
/// 在两台机器上长得不一样**，而这是本项目唯一一处主题依赖 —— 面板的底色、每个字、
/// 每根折线都是自己给的色值，只有这条线在跟着系统跑。
const RULE_COLOR: egui::Color32 = egui::Color32::from_rgb(0xBE, 0xBE, 0xBE);

/// 会话之间那条分隔线，**零占位、颜色自己给**。
///
/// 为什么不用 `egui::Separator`：
/// 1. 它自带一条 6pt 高的占位带（`spacing` 默认 6）、线画在正中间，于是"第二行 → 线"
///    会凭空多出 3pt —— 间距常量就不再等于界面上的数。与 [`row_line`] 钉死行高是同一个
///    目的：**凡是"间距"就不允许有隐式份量。**（原先靠 `spacing(0.0)` 取掉那条带。）
/// 2. **它的颜色没有任何参数可传**（`Separator` 结构体只有 `spacing` / `grow` /
///    `is_horizontal_line` / `classes`，没有 `stroke`），只能靠改写 `ui.visuals_mut()`
///    去影响它 —— 而那是**全局**的：同一个 `Ui` 上将来任何非交互控件都会连带变色。
///    自己画则颜色就地写死，见 [`RULE_COLOR`]。
///
/// 几何与原先逐点一致（`Separator` 内部也是 `painter.hline(rect 左..右, rect 中线 y)`），
/// 所以 [`EXTRA_ROW_GAP`] 与四段纵向间距里量到的数都不会动 ——
/// `the_four_vertical_gaps_are_the_ruled_ones` / `rows_are_spaced_twice_as_far_apart_as_before`
/// 就是这两条的守卫。
fn rule(ui: &mut egui::Ui) {
    // ⚠️ 高度**必须是 0**（与 `Separator::spacing(0.0)` 的 `vec2(w, 0.0)` 逐点一致）：
    // 取 `RULE_WIDTH` 当高度会让游标多走 1pt、线也整体下沉 0.5pt，四段纵向间距就会全线 +1
    // （`EXTRA_ROW_GAP` 的 58 是**文字原点差**，量得出来）。
    let (rect, _) = ui.allocate_at_least(
        egui::vec2(ui.available_width(), 0.0),
        egui::Sense::hover(),
    );
    ui.painter().hline(
        rect.left()..=rect.right(),
        rect.center().y,
        egui::Stroke::new(RULE_WIDTH, RULE_COLOR),
    );
}

/// 会话行里"今日 token"那一格的字号与颜色。
///
/// 放在**第二行**（见 `draw_row` 里的说明：放第一行会把会名挤没，实测过）。
///
/// 字号**与同行的 `上下文 N%` 一致**（= `CONTEXT_SIZE`），这不是审美而是**代价**：
/// 同一行里两个字号不一致时，行高由**大的那个**说了算（见 `draw_row` 的 `ctx_row_h`），
/// 小的那个一分钱不省、还平白多一种字号要维护。这条是被一次试做钉下来的 ——
/// 当初把这格单独设成 15pt，行高直接从 8pt 涨到 16pt，`ROW_LINE_GAP` /
/// `ROW_BOTTOM_GAP` / `EXTRA_ROW_GAP` 连带都要重标。**用同一号字则不额外收费**：
/// 行高只由 `CONTEXT_SIZE` 决定、四段间距一个都不用动。
///
/// ⚠️ 那次 15pt 试做留在注释里的数字（"14 变 18"）**别再引用**：它与本次实测的口径对不上
/// （本次 7.5 → 11.25pt 实测：这一行 galley 高 8 → 12、会话块 44 → 48、会话间距 58 → 62，
/// 而"第一行→第二行"仍是 14 —— 那是 `add_space`，不随字高走）。同一个道理、另一组数。
///
/// 颜色取中性灰而不是纯黑：黑留给会名（主信息）。
const TOKENS_SIZE: f32 = CONTEXT_SIZE;
const TOKENS_COLOR: egui::Color32 = egui::Color32::from_rgb(0x5a, 0x5a, 0x5a);

/// 顶栏那条**不含数码子集**的字体族名。用户 2026-09-15 裁定："大标题里的所有字体都
/// 统一，不用数码"。
///
/// **为什么需要单独一条族**：字体栈是逐字符回退的，数字段只要子集在栈里就会被它截走
/// （顶栏的 `3` 本来正是这么变成数码体的）。想让某段文字**避开**数码体，唯一的办法是
/// 给它一条不含子集的族 —— 这是本文件顶部"不选路线 A（两个族 + `FontId`）"那段注释的
/// 一个例外：那处回避 `FontId` 是为了不让**每一处**画字都手工切段，而这里是**整条顶栏
/// 统一避开**，一处指定即可，代价只是一条族。
const TEXT_FAMILY: &str = "claude-hud-text";

/// 宿主 Claude Code 进程已消失时显示的状态词（9b；用户 2026-09-15 裁定的措辞）。
///
/// 它**取代** `state` 对应的词，而不是并列 —— 这个会话的真实处境是"它已经不在了"，
/// 而不是它最后一个 hook 事件报的那个状态（那个状态会永远停在那里，因为不会再有事件了）。
const EXITED_LABEL: &str = "已退出";
/// "已退出"的颜色：中性灰，读起来就是"不活跃"。与 `State::Interrupted` 的灰（`0x757575`）
/// 刻意错开一点，免得两种灰在同一屏里被看成同一个东西。白底对比度 5.3:1（过 AA）。
const EXITED_COLOR: egui::Color32 = egui::Color32::from_rgb(0x6b, 0x6b, 0x6b);

/// 默认窗口尺寸（用户两次上调：240 → 360 → 380）。
///
/// 提到 380 的理由是**用户声明的上限 = 4 条会话**：一条会话 44pt 高、相邻两条相隔 94pt，
/// 4 条需要 `41 + 3×94 + 44 = 367pt`，加下内边距 ⇒ 380 才装得下。
/// 由 `four_sessions_all_fit_at_the_default_size` 守着。
const DEFAULT_INNER_W: f32 = 320.0;
const DEFAULT_INNER_H: f32 = 380.0;

/// 窗口**最小宽度**。**这个值现在只由"会名还剩几个字"决定。**
///
/// ⚠️ 它原来记的三条依据（"<191 第二行压过黑框 / <181 被右缘裁 / **<168 连计时都被裁**"）
/// **全部作废**，别再引用：那三条是 `CONTEXT_SIZE` 还是 15pt、且**第二行与计时都还是
/// 顺序左排**的年代量的。2026-09-15 第三轮立了右缘基准之后，计时与状态词由
/// [`paint_right`] **画在离右缘固定的位置**（不再参与布局），实测在 140pt 宽下
/// **右缘仍精确落在内容右界上** —— "计时被裁"这条下界已经不存在了。
///
/// 现在**右端**永不退化；**左端**还有两条线（2026-09-16 复审在真帧上量出来的）：
/// 约 **190pt** 以下状态词开始与会名的槽位抢地方、约 **125pt** 以下整组画出面板
/// （120pt 时「已退出」已经压到描边上、「等待确认」跑到窗口外）。**这个下限同时挡住了这两条。**
///
/// 会名那一维（最坏情况：`示例方法论系统` + `等待确认` + 一个 **5 字符**计时 + `子代理 12`）：
///
/// | 窗宽 | 会名 |
/// |---|---|
/// | 170 | **整个消失** |
/// | 190 | `…` |
/// | 200 | `示…` |
/// | 220 | `示例…` |
///
/// ⚠️ **这张表原先只写了"5 字符计时"，却没写它会变长** —— `fmt_elapsed` 改前是
/// `{:02}:{:02}(分)`、**分钟不封顶**，跑过 100 分钟就多一位，**同样是 220pt 会名掉成 `示…`**。
/// 2026-09-21 在真帧上逐格量了这一列：
///
/// | 窗宽 | 5 字符（`59:59`） | 6 字符（`999:59`） |
/// |---|---|---|
/// | 220.0 | `示例…` | **`示…`** |
/// | 221.0 | `示例…` | **`示…`** |
/// | 221.5 | `示例…` | `示例…` |
/// | 222.0 | `示例…` | `示例…` |
/// | 226.0 | `示例方…` | `示例…` |
///
/// ⇒ 6 字符那一列第一个够宽的宽度落在 **221～221.5 之间**，取整就是 **222**（由 220 上调 2pt）。
/// 这 2pt **只影响"窗口能拖到多窄"这一个边界**：默认 320pt 与任何实际使用宽度都不受影响。
///
/// **计时本身也改了**（用户同一天裁定）：满 1 小时从 `MM:SS` 换成 `H:MM`，于是
/// `8368:11` 那种既长又读不懂的数变成 `139:28`。**长度上界只是被推远、没被消灭** ——
/// `H:MM` 在 100 小时（4.2 天）后才变 6 个字符、1000 小时（41.7 天）后变 7 个。
/// **222 这个数就是按 6 个字符定的 ⇒ 它覆盖到 41.7 天**，在此之前不管会话开多久都成立。
/// 见 [`fmt_elapsed`]。
///
/// 📌 **这 2pt 是量出来的，别照抄第一版**：一开始只采了 220 / 228 两个宽度就报了 228
/// （"补 8pt"），细采之后真实门槛在 221.2 附近 —— **两点之间的东西全是猜的**。
/// 这正是本仓"实测数字不保鲜"的老毛病。
///
/// **两列都有测试守着**：`the_minimum_width_is_the_smallest_width_that_still_shows_two_name_characters`
/// 逐格钉 5 字符那张表；`a_long_timer_does_not_steal_the_second_name_character`
/// **搜出** 6 字符下第一个够宽的宽度再与本常量比对（不是"在 222 上断言两个字"——
/// 那种写法在字体变窄之后照样绿）。改字号/字体/额度公式之前先看这两条。
const MIN_INNER_WIDTH: f32 = 222.0;
/// 窗口**最小高度**：**顶栏 + 一条完整会话**刚好放得下的那个值。
///
/// 用户 2026-09-17："贴边就能拖，**并且设置最小的大小限制**" —— 下限的判据由此明确：
/// 不是"别压成一条缝"，而是"**至少得看清一条会话**"。
///
/// 构成（都是本文件里已实测的量）：
/// ```text
/// 面板上下内边距  8 + 8 = 16
/// 顶栏          17pt 字块 + 分隔线 + HEADER_GAP 16   ≈ 44
/// 一条会话      行高 22 + ROW_LINE_GAP 14 + 第二行 8 + ROW_BOTTOM_GAP 8 + EXTRA_ROW_GAP 42 = 94
/// ```
/// ⇒ 16 + 44 + 94 = **154**，取 **160** 留 6pt 余量（字体或间距若微调，不至于立刻切掉）。
///
/// ⚠️ 旧值 120 是**没有依据**的（注释里当时就写着"从来没实测过"），
/// 它连"顶栏 + 一条会话"都放不下（154 > 120）—— 拖到下限时看到的是**半条被切掉的会话**。
/// 由 `the_minimum_height_fits_the_header_and_one_whole_session` 守着。
const MIN_INNER_HEIGHT: f32 = 160.0;

pub fn run() -> eframe::Result<()> {
    let cfg = config::load_from(&paths::real_config_path());

    // 中文字体**先探一次**（纯文件 IO，见 `pick_font`）。探测与安装分开，是因为
    // "取不到中文字体"这件事必须在**窗口出现之前**说出来；而安装要等 eframe 建好窗口
    // （`set_fonts` 是 `Context` 的方法）。
    //
    // 取不到时的表现是整屏缺字方框 —— **不 panic、不报错**，正是 spec §4.3 点名的
    // 失效模式；而同一条启动路径上 `main.rs` 已经为"配置读不出来"准备了对话框，
    // 这里没有理由沉默。（`install_fonts` 的返回值原先被直接丢弃、注释却写着"供启动
    // 诊断"，而那个诊断并不存在。这就是它的落实。）
    let cjk = pick_font(&font_candidates());
    if cjk.is_none() {
        crate::dialog::show_message(
            "claude-hud · 中文字体缺失",
            &no_cjk_message(&font_candidates()),
            true,
        );
    }

    let mut viewport = egui::ViewportBuilder::default()
        // 默认尺寸 **320×380**（240 → 360 → 380，两次上调都因为"装不下几条会话"）。
        //
        // 2026-09-17 从 360 提到 380：用户报"四个会话为什么只显示了 3 个" —— 实测
        // 4 条会话需要 **41 + 3×94 + 44 = 367pt**（一条会话 44pt 高、相邻两条相隔 94pt），
        // 而 360 的内容区只有 344pt。**用户明确说过"我最多开四个"**，所以默认尺寸的判据
        // 就是"四条要装得下"，取 380 留 13pt 余量。
        .with_inner_size([DEFAULT_INNER_W, DEFAULT_INNER_H])
        // 用户可以把窗口拖到任意窄，而内容不会跟着缩 —— 窄到一定程度会名就没了。
        // 设下限是修这个（第三轮之后只有会名会退化，状态词与计时由 `paint_right`
        // 画在固定位置、不参与布局，窄到什么程度都不会被裁。详见 `MIN_INNER_WIDTH`）。
        .with_min_inner_size([MIN_INNER_WIDTH, MIN_INNER_HEIGHT])
        .with_decorations(false);
    // 默认的 window_level 是 None（即 Normal）。只有配置要求置顶时才设成
    // AlwaysOnTop —— 无条件调用会把 "always_on_top": false 这个配置项吃掉。
    // （egui 0.36 没有收 bool 的 `with_always_on_top(bool)`，只有无参版本。）
    //
    // 窗口**打开透明**，但**不是**让面板半透明 —— 这两件事必须分清：
    //
    // - 用户 2026-09-14 撤销的是"**面板背景半透明**"（连带删掉了 `opacity` 配置项）：
    //   面板填充至今仍是 `Color32::WHITE`，**alpha = 255**（`panel_frame_is_white_filled_...` 钉着）。
    // - 用户 2026-09-16 要的是"**挂件改为圆角**"，而圆角**只能**画在透明窗口上：
    //   不透明的窗口里画圆角，那四个角只会露出窗口自己的底色（一块方角），等于没做。
    //
    // 于是透明只发生在**圆角切掉的那四小块**上，其余整块面板照旧纯白不透明。
    // `App::clear_color` 必须一起给全透明，否则窗口底色会把那四个角填成不透明的。
    viewport = viewport.with_transparent(true);
    if cfg.always_on_top {
        viewport = viewport.with_always_on_top();
    }
    // 位置记忆（用户 2026-09-16 批准做的第一件）。改之前 `window_pos` 是**死配置**：
    // 有字段、无读者，拖完位置一重启就回到系统随手放的地方。
    //
    // 只在坐标**可信**时才用（见 `config::plausible_window_pos`）—— 一个落在屏幕外的
    // 坐标会让这个无边框窗口再也找不回来。
    if config::plausible_window_pos(cfg.window_pos) {
        viewport = viewport.with_position(cfg.window_pos);
    }

    let opts = eframe::NativeOptions { viewport, ..Default::default() };

    eframe::run_native(
        "claude-hud",
        opts,
        Box::new(move |cc| {
            // `set_fonts` 是 `Context` 的方法，所以只能等 eframe 建好窗口再装；
            // 候选的挑选与读取已在 `run` 开头做完（那一步要早于窗口，见那里的说明）。
            let bytes = cjk.map(|(_, bytes)| bytes);
            cc.egui_ctx.set_fonts(build_font_definitions(bytes));

            let snapshot: Snapshot = Arc::new(Mutex::new(SnapshotData::default()));
            let alive = Arc::new(AtomicBool::new(true));
            let worker = spawn_poller(
                cfg.clone(),
                Arc::clone(&snapshot),
                cc.egui_ctx.clone(),
                Arc::clone(&alive),
            );

            Ok(Box::new(App {
                saved_pos: cfg.window_pos,
                cfg,
                snapshot,
                alive,
                worker: Some(worker),
                last_pos: None,
                pos_stable_since: None,
                resizing: None,
            }))
        }),
    )
}

// ---- 字体（数字走数码体 + 其余走微软雅黑）-------------------------------------
// 用户 2026-09-14 第二轮裁定："**数字用数码风格就好，其他的用微软雅黑**"。
// 起因是上一版把整条 ASCII 都交给了 DSEG14，于是 `Claude`、`web-lab` 也变成
// 数码管字形，用户觉得"不太好看"。
//
// **路线选择（B：字符子集）与理由。** 字体栈按序装成下面这条（比例族与等宽族
// 各一条，顺序相同）：
//
//     [claude-hud-digits] → [cjk] → [egui 自带：Ubuntu-Light / NotoEmoji / emoji-icon]
//
// `claude-hud-digits` 是 DSEG14 的**字符子集**，只留 `0-9` 与 `:`。于是"数字走
// 数码体、其余一律走微软雅黑"这条规则**在任意混合串里自动成立** —— egui 对族内
// 字体是**逐字符**回退的（中文一直以来就是这么落到 cjk 上的），所以：
//
//   · 计时 `02:13`        → 每个字符都在子集里 → 全是数码体
//   · `上下文 68%`         → 中文与 `%` 不在子集 → 微软雅黑；`68` → 数码体
//   · `● claude 会话 · 3 个` → 只有 `3` → 数码体，其余微软雅黑
//
// 另一条已知可行的路线是 **(A) 建两个字体族 + 在数字段上用 `FontId`**（egui 的
// `LayoutJob` 支持一段文本内按 section 混用字体）。**不选它的理由**：那样每一处
// 画字的地方都要手工把字符串切成段、逐段指定族，`text_width` / `truncate_to_width`
// 也得跟着改成"按段量宽再拼"，而**任何一处漏切都会静默地退回"整串一个字体"**。
// 子集路线只改一个地方（字体栈本身），混合串的正确性由 egui 的回退链在构造上保证，
// 不需要逐处维护。代价是**多了一份派生字体**（18 KB，已提交进仓库）。
//
// ⚠️ 子集是**派生字体**：OFL 1.1 条件 3 禁止派生版本沿用保留字体名 "DSEG"，故已
// 改名 `claude-hud-digits`，且仍整个按 OFL 1.1 分发 —— 改了什么、授权如何随附、
// 怎么从原版重放，全部写在 `assets/claude-hud-digits-NOTICE.txt`。子集是**一次性
// 离线产物**（fontTools，2026-09-14 跑过一次），**cargo 构建不依赖 Python**。

/// 数码**子集**（只含 `0`-`9` 与 `:`）**直接嵌进二进制**，不是运行时从磁盘读：
/// 便携版换台机器、或工作目录变了，字体都还在。
///
/// 字形轮廓与水平步进与原版 DSEG14 **逐字相同**（`0`-`9` = 816/1000 em、
/// `:` = 200/1000 em，已用 fontTools 比对），所以子集里的数字与"原版独装"同宽。
const DIGITS_BYTES: &[u8] = include_bytes!("../assets/claude-hud-digits.ttf");
const DIGITS_NAME: &str = "claude-hud-digits";
const CJK_NAME: &str = "cjk";

/// 候选中文字体，按序尝试。**微软雅黑排第一**：用户点名"其他的用微软雅黑"。
///
/// `.ttc`（字体集合）不再排到最后 —— 真机实测 egui 0.36（走 harfrust/skrifa 的
/// `FontRef::from_index`）**能**吃 index 0 的 `.ttc`，而本机的微软雅黑正是
/// `msyh.ttc`（index 0 = "Microsoft YaHei" Regular，已实测）。
/// 单文件 `msyh.ttf`（Win7 时代）排在第二位，只是老机器的兜底。
/// 最后是 `segoeui.ttf`：它不含中文，只在"所有中文字体都取不到"时兜底，
/// 至少保证拉丁字符正常而不是整体缺字。
///
/// ⚠️ 这张表**只管中文**。它不是整个字体栈 —— 栈的第一项是内嵌的数码子集，
/// 由 `build_font_definitions` 无条件插在最前（那是编译期就保证可用的）。
fn font_candidates() -> Vec<PathBuf> {
    let win = PathBuf::from("C:\\Windows\\Fonts");
    [
        "msyh.ttc",    // 微软雅黑（集合，index 0 = Microsoft YaHei）—— 用户点名的首选
        "msyh.ttf",    // 微软雅黑（单文件版，Win7 时代才有）
        "Deng.ttf",    // 等线（取不到雅黑时的次选，Win10+ 自带）
        "simhei.ttf",  // 黑体
        "simkai.ttf",  // 楷体
        "simfang.ttf", // 仿宋
        "simsun.ttc",  // 宋体（集合）
        "segoeui.ttf", // 无中文，最后兜底
    ]
    .iter()
    .map(|f| win.join(f))
    .collect()
}

/// 校验 sfnt 头，判断这段字节**值不值得交给 egui**。
///
/// egui 解析字体失败时是 `panic!`（`epaint-0.36.2/src/text/fonts.rs:990`），不是回退；
/// 而挂件的 `#![windows_subsystem = "windows"]` 会让这个 panic **无声无息**（没有控制台、
/// 没有对话框，用户看到的是"双击了，什么都没发生"）。
///
/// **只查前 4 字节 magic 是不够的** —— 这是本函数第二版修掉的洞：截断**恰恰保留**
/// magic。一份被写坏成 ≤32 字节残桩的 `msyh.ttc` 仍以 `ttcf` 开头，会被放行，然后在
/// 第一帧 panic（实测：截到 4/12/16/32 字节都 panic，64 字节以上才是静默退化）。
/// **旧注释把"截断文件"写成已挡住，实际没挡** —— 当时的测试也都只喂 4 字节 magic 的
/// 假字体，所以没有任何一条能发现这件事。
///
/// 所以这里连**长度与表目录的自洽性**一起查（纯字节运算，不做完整解析）：
///
/// - `\x00\x01\x00\x00` / `OTTO` / `true`：偏移 4 是 `numTables`（大端 u16），表目录
///   每项 16 字节 ⇒ 需要 `numTables > 0` 且 `len >= 12 + 16 * numTables`。
/// - `ttcf`（字体集合）：偏移 8 是 `numFonts`（大端 u32），其后紧跟 `numFonts` 个
///   4 字节字体偏移 ⇒ 需要 `numFonts > 0`、偏移表本身放得下、且每个偏移都指向文件内
///   一个能放下 12 字节 sfnt 头的位置。
///
/// **不保证穷尽**：长度充足但表目录内容错乱的文件仍可能在解析时 panic。要穷尽得把
/// egui 的解析器引进来预跑一遍 —— 那要多一个依赖，与本项目"不留不兑现的依赖"的口径
/// 不符。那一格由 `main.rs` 的全局 panic 兜底（把无声死亡变成可见弹框）。
fn looks_like_font(bytes: &[u8]) -> bool {
    match bytes.get(..4) {
        Some(b"ttcf") => ttc_header_is_coherent(bytes),
        Some(b"\x00\x01\x00\x00" | b"OTTO" | b"true") => sfnt_header_is_coherent(bytes),
        _ => false,
    }
}

/// `at` 处的大端 u16；越界或 `at + 2` 溢出都返回 `None`。
fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let b = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

/// `at` 处的大端 u32；越界或 `at + 4` 溢出都返回 `None`。
fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// 单个 sfnt 字体（TrueType / CFF / Apple）的头部自洽性。
fn sfnt_header_is_coherent(bytes: &[u8]) -> bool {
    match be_u16(bytes, 4) {
        Some(n) if n > 0 => 12usize
            .checked_add(16 * n as usize)
            .is_some_and(|need| need <= bytes.len()),
        _ => false,
    }
}

/// 字体集合（TTC）的头部自洽性。
fn ttc_header_is_coherent(bytes: &[u8]) -> bool {
    let Some(n) = be_u32(bytes, 8).and_then(|n| usize::try_from(n).ok()) else {
        return false;
    };
    if n == 0 {
        return false;
    }
    // 字体偏移表本身要放得下（`4 * n` 溢出即判否）。
    let Some(dir_end) = 4usize.checked_mul(n).and_then(|d| 12usize.checked_add(d)) else {
        return false;
    };
    if dir_end > bytes.len() {
        return false;
    }
    // 每个偏移都要指向文件内、且放得下一个 12 字节 sfnt 头。
    (0..n).all(|i| {
        be_u32(bytes, 12 + 4 * i)
            .and_then(|off| usize::try_from(off).ok())
            .and_then(|off| off.checked_add(12))
            .is_some_and(|end| end <= bytes.len())
    })
}

/// 从候选里挑**第一个能读且像是字体**的，返回路径与字节。全失败返回 `None`
/// （调用方回落 egui 默认字体，不得 panic）。
///
/// 抽成纯函数是为了可测：真实的 `C:\Windows\Fonts` 不进单测，用临时文件喂候选。
pub fn pick_font(candidates: &[PathBuf]) -> Option<(PathBuf, Vec<u8>)> {
    for p in candidates {
        let Ok(bytes) = std::fs::read(p) else { continue };
        if looks_like_font(&bytes) {
            return Some((p.clone(), bytes));
        }
    }
    None
}

/// 组装字体栈：`[digits] → [cjk?] → [egui 自带…]`，**比例族与等宽族各一条**。
/// 只挂比例族的话，等宽文本里的中文仍是方框。
///
/// 顺序就是"数字走数码、其余走微软雅黑"这句话的全部实现：**逐字符**回退，
/// 子集里有的字符（`0-9` `:`）被它在 0 位截住，其余一律落到后面的中文/拉丁字体。
///
/// `cjk` 为 `None`（一个中文字体都取不到，例如非中文 Windows）时**仍然装数码子集**
/// —— 它是内嵌的、永远在手上；此时中文会显示成缺字方框，但**不能因此 panic 或
/// 退回 egui 默认**，那会把数码头一起丢掉（计划的既定行为是"取不到就静默回落"）。
fn build_font_definitions(cjk: Option<Vec<u8>>) -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    // 先留一份 egui 自带的族顺序：顶栏那条族要在它基础上**只加 cjk、不加数码子集**。
    // 必须在下面往 families 里插东西**之前**取。
    let builtin_prop = fonts.families[&egui::FontFamily::Proportional].clone();

    fonts.font_data.insert(
        DIGITS_NAME.to_owned(),
        Arc::new(egui::FontData::from_static(DIGITS_BYTES)),
    );
    let has_cjk = cjk.is_some();
    if let Some(bytes) = cjk {
        fonts.font_data.insert(
            CJK_NAME.to_owned(),
            Arc::new(egui::FontData::from_owned(bytes)),
        );
    }

    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let list = fonts.families.entry(fam).or_default();
        // 先插中文、再插数码子集，两次都插在 0 位 ⇒ 最终顺序
        // [digits, cjk, <egui 自带…>]，即"数字优先数码，其余优先中文/拉丁"。
        if has_cjk {
            list.insert(0, CJK_NAME.to_owned());
        }
        list.insert(0, DIGITS_NAME.to_owned());
    }

    // 顶栏族：顺序与主族一致（cjk 在前、egui 自带殿后），**独独不放数码子集** ——
    // 于是它里面的数字走 cjk/egui 自带，不再是数码管字形。见 `TEXT_FAMILY`。
    let mut text_family = builtin_prop;
    if has_cjk {
        text_family.insert(0, CJK_NAME.to_owned());
    }
    fonts
        .families
        .insert(egui::FontFamily::Name(TEXT_FAMILY.into()), text_family);

    fonts
}

/// "一个中文字体都取不到"那条对话框的正文。
///
/// 抽成纯函数是为了**可测**：`show_message` 本身要人点、测不了，但"候选路径有没有列全"
/// 是纯字符串 —— 没有理由不钉住（与账本第 77 条同一类：9c 的错误文案也是可测的）。
fn no_cjk_message(candidates: &[PathBuf]) -> String {
    format!(
        "本机一个中文字体都取不到，界面上的中文（状态词、中文会话名）会显示成缺字方框。\n\n\
         已按序试过这些候选：\n{}\n\n\
         数字与计时不受影响 —— 数码字体是内嵌在程序里的。",
        candidates
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

// （原先这里有个 `install_fonts(ctx, candidates) -> Option<PathBuf>`：自己探测、自己
// 安装、把用上的字体路径返回给一个**并不存在**的"启动诊断"。本轮把它拆开了 ——
// `run` 先 `pick_font` 探一次（好在窗口出现**之前**报"中文会变方框"，见那里的说明），
// 再在 eframe 的闭包里 `set_fonts`。函数因此没有留下的必要，删掉而不是留个转发壳。）

// ---- 轮询（后台线程）----------------------------------------------------------

/// 一轮轮询：列出状态目录 → 刷新每个会话的 transcript 派生信息 → 生成行。
///
/// ⚠️ 控制器裁定（Ruling #6，pre-flight）：**挂件绝不写状态文件。**
/// 每个状态文件只能有一个写者（hook 侧）。挂件手里的内存副本可能是旧的
/// （state=working），回写会覆盖 hook 刚写下的 state=waiting —— 原子写只
/// 保证不读到半截文件，不保证不丢更新。故 offset 与 Delta 一样只活在挂件
/// 进程内存里；重启后重扫一次 transcript 作一次性开销。
///
/// `offsets` 因此**必须由调用方跨轮次持有**：状态文件每轮都从磁盘重新读，
/// 而挂件不写回，所以盘上的 `transcript_offset` 恒为 0。若不复用内存里的
/// offset，每轮都会从头重读整份 transcript —— 实测 5 个会话 88 MB 每轮 730 ms，
/// 等于每秒白烧 0.7 秒 IO（这是"界面卡顿"的真正主因，不是只有第一轮慢）。
fn poll_once(
    dir: &Path,
    deltas: &mut HashMap<String, transcript::Delta>,
    offsets: &mut HashMap<String, u64>,
    dead_streak: &mut HashMap<String, u32>,
    cfg: &config::Config,
    now: i64,
) -> Vec<view::Row> {
    let mut states = state::list_all(dir);

    // 子会话（被另一个 Claude 会话拉起的 `claude -p` 子进程）**不列出来**。
    //
    // 用户 2026-09-18 裁定。根因：真机上那个子进程自己也是一个会话、也写状态文件，
    // 于是挂件把 token 实验的两个 arm 当成用户开的会话各列一行 —— **用户开 4 个，
    // 界面显示 6 个**（用户原话："我明明是四个却显示六个"）。它们是父会话干的活，
    // 由父会话那一行体现；挂件的头等职责是"我有几个会话"，多出来的行直接把这件事说错。
    //
    // 这个事实由 **hook 在 `SessionStart` 定好**（`state::SessionState::nested`），
    // 此处只读 —— 绝不在轮询热路径上再枚举一次进程表（7.5 ms/次，见 `procinfo` 顶部）。
    //
    // ⚠️ **只有 `Some(true)` 才藏**：`None` = 改造前留下的状态文件、没有这个事实，
    // 按"用户自己开的"处理。**宁可多列一行，不可把真会话藏起来。**
    //
    // 放在刷新 transcript **之前**：子会话的转录不必解析（它的行根本不画），
    // 而它可能很大 —— 实验 arm 的转录动辄几 MB。
    states.retain(|s| s.nested != Some(true));

    for s in states.iter_mut() {
        s.transcript_offset = offsets.get(&s.session_id).copied().unwrap_or(0);
        let _ = poller::refresh(s, deltas, now);
        offsets.insert(s.session_id.clone(), s.transcript_offset);
    }

    // 9b 的僵尸会话：宿主进程还在不在。`dead_streak` 与 `offsets` 一样跨轮持有 ——
    // 状态文件每轮从磁盘重读、而挂件不写回，所以"已经死了几轮"只能记在内存里。
    // 用户 2026-09-15 裁定：**先标"已退出"，连续两轮确认后再移除** —— 一次误判只让标签
    // 变一下，不会让一个活着的会话从界面上消失。
    let mut gone: HashSet<String> = HashSet::new();
    for s in states.iter() {
        let streak = dead_streak.entry(s.session_id.clone()).or_insert(0);
        *streak = if host_is_gone(s) { *streak + 1 } else { 0 };
        if *streak >= 1 {
            gone.insert(s.session_id.clone());
        }
    }

    let mut rows = view::build_rows_with_deltas(&states, deltas, cfg, now);
    rows.retain(|r| dead_streak.get(&r.session_id).copied().unwrap_or(0) < 2);
    view::mark_host_gone(&mut rows, &gone);
    rows
}

/// 会话的宿主 Claude Code 进程是否已经不在了。
///
/// **`claude_pid` 为 `None` 时一律返回 `false`（不做判定）**：那可能是改造前留下的状态
/// 文件、或 `SessionStart` 时没抓到（快照失败、权限不足）。此时退回改造前的行为 ——
/// **宁可漏报僵尸，不可误杀活人。**
///
/// "判不准"与"判为在"落在同一个分支，这是刻意的，理由见 `procinfo::alive` 的注释。
fn host_is_gone(s: &state::SessionState) -> bool {
    match s.claude_pid {
        Some(pid) => !procinfo::alive(pid, procinfo::HOST_EXE),
        None => false,
    }
}

/// 起一个后台轮询线程：每轮把新快照写进 `snapshot`，然后唤醒 UI 重绘。
/// `poll_interval_ms` 仍是配置项，没有写死。
fn spawn_poller(
    cfg: config::Config,
    snapshot: Snapshot,
    ctx: egui::Context,
    alive: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let dir = paths::real_sessions_dir();
        let mut deltas: HashMap<String, transcript::Delta> = HashMap::new();
        let mut offsets: HashMap<String, u64> = HashMap::new();
        let mut dead_streak: HashMap<String, u32> = HashMap::new();
        // 今日 token 账本：**同一个后台线程**里增量扫（只读新增字节、mtime 预筛），
        // 与状态轮询共享一次唤醒 —— 不必再起第二个线程，也就没有第二把锁。
        let mut scanner = usage::Scanner::new(paths::real_projects_dir());
        let interval = Duration::from_millis(cfg.poll_interval_ms.max(1));
        // 睡眠切片：退出时不必等满一个周期（见 on_exit 的 join）
        let slice = Duration::from_millis(50).min(interval);

        while alive.load(Ordering::Relaxed) {
            let now = chrono::Local::now().timestamp();
            let rows = poll_once(&dir, &mut deltas, &mut offsets, &mut dead_streak, &cfg, now);
            scanner.scan(usage::today());
            if let Ok(mut slot) = snapshot.lock() {
                // ⚠️ 交给 UI 的是**视图**而不是整本账：整本账每帧克隆要 172 µs，
                // 而 UI 只读其中三样（1.8 µs），差 97 倍 —— 见 `usage::Ledger::view`。
                *slot = SnapshotData {
                    rows,
                    ledger: scanner.ledger().view(),
                    // 走到这里说明**这一轮扫描已经完成**（哪怕它一条会话都没扫到）。
                    first_scan_done: true,
                };
            }
            if !alive.load(Ordering::Relaxed) {
                break;
            }
            ctx.request_repaint(); // 有新数据就唤醒 UI，不必等它自己到点

            let mut slept = Duration::ZERO;
            while slept < interval && alive.load(Ordering::Relaxed) {
                std::thread::sleep(slice);
                slept += slice;
            }
        }
    })
}

// ---- 窗口拖动 ----------------------------------------------------------------
// `with_decorations(false)` 去掉了标题栏 → 系统不再提供任何可拖区域，
// 不发 `ViewportCommand::StartDrag` 窗口就只能钉在原地。
//
// 为什么是这个 API：0.36 的 `ViewportBuilder` **没有** `with_draggable` 之类的
// builder 开关（只有 `with_decorations`），可拖动性只能靠运行期发命令。
// `StartDrag` 的文档明确要求"调用前左键必须刚按下"，所以在 `drag_started()`
// 那一帧发正是时机（`drag_started` 只在拖拽开始的那一帧为真）。
/// 边缘缩放热区的**带宽**（pt）。
///
/// 无边框窗口（`with_decorations(false)`）**没有系统给的缩放热区**，得自己划。
/// 不能太窄 —— 真机上 4pt 很难精准命中；也不能太宽 —— 它会从拖动区里挖掉一圈，
/// 太宽就不方便拖窗口了。**8pt**（2026-09-17 从 6 加宽：用户要"贴边就能拖"，
/// 而 6pt 那版还得往里凑一点才好命中）。
const RESIZE_BORDER: f32 = 8.0;

/// 指针 `p` 压在 `rect` 的哪条边/哪个角上；不在边缘带里返回 `None`。
///
/// 纯函数，所以能单测 —— 这是这一块唯一能自动验证的部分。**窗口真的被缩放**是发给
/// 窗口系统的命令，只能人工验收（与拖动同理，见下面那条注释）。
/// 位置稳定多久之后才回写配置（秒）。
///
/// 拖拽过程中每帧都会变；不等它停下来就写，会在一次拖拽里产生几十次写盘。
/// 1 秒足够区分"正在拖"与"拖完了"。
const SAVE_POS_AFTER_SECS: f64 = 1.0;

/// 位置移动多少才算"真的移动了"（pt）。小于它的抖动不值得写盘。
const POS_EPSILON: f32 = 1.0;

/// 现在该不该把窗口位置写回配置？**纯函数**，所以这条判据可以被单测钉住
/// （真窗口的拖动只能人工验收，理由与 `resize_direction_at` 同）。
///
/// 三个条件缺一不可：**停稳了**（`stable_for ≥ 1s`）、**真的动了**（超出 `POS_EPSILON`）、
/// **坐标可信**（不是 NaN / 天文数字）。只判"动了"会让拖拽过程疯狂写盘；
/// 只判"停稳"会在没动时也写一遍；不判可信会把屏幕外的坐标记下来，下次窗口就找不回来了。
fn should_save_pos(cur: [f32; 2], saved: [f32; 2], stable_for: f64) -> bool {
    if stable_for < SAVE_POS_AFTER_SECS {
        return false; // 还在拖（或刚停下）
    }
    let moved = (cur[0] - saved[0]).abs() > POS_EPSILON || (cur[1] - saved[1]).abs() > POS_EPSILON;
    moved && config::plausible_window_pos(cur)
}

/// **窗口矩形**（egui 坐标系，原点在窗口左上角）。
///
/// 面板（`CentralPanel` + `panel_frame`）铺满整个窗口，所以"内容区外扩一个内边距"
/// 就是窗口矩形。**不要**去用 `viewport().inner_rect` —— 那个是**屏幕坐标**（含窗口在桌面上的
/// 位置），拿它当 `ui.interact` 的矩形会一个点都命中不了（窗口位置是 2000+ 的时候尤其明显）。
///
/// 抽成纯函数是为了能被单测钉住：它是"最边上能不能按到"的关键那一环。
fn window_rect(ui: &egui::Ui) -> egui::Rect {
    ui.max_rect().expand(INNER_MARGIN as f32)
}

fn resize_direction_at(
    rect: egui::Rect,
    p: egui::Pos2,
) -> Option<egui::viewport::ResizeDirection> {
    use egui::viewport::ResizeDirection as Dir;
    // 按下点可能**略微落在窗口外**（描边外沿），所以判定前把 rect 外扩一个带宽。
    if !rect.expand(RESIZE_BORDER).contains(p) {
        return None;
    }
    let (west, east) = (
        (p.x - rect.left()).abs() <= RESIZE_BORDER,
        (p.x - rect.right()).abs() <= RESIZE_BORDER,
    );
    let (north, south) = (
        (p.y - rect.top()).abs() <= RESIZE_BORDER,
        (p.y - rect.bottom()).abs() <= RESIZE_BORDER,
    );
    match (north, south, west, east) {
        (true, _, true, _) => Some(Dir::NorthWest),
        (true, _, _, true) => Some(Dir::NorthEast),
        (_, true, true, _) => Some(Dir::SouthWest),
        (_, true, _, true) => Some(Dir::SouthEast),
        (true, _, _, _) => Some(Dir::North),
        (_, true, _, _) => Some(Dir::South),
        (_, _, true, _) => Some(Dir::West),
        (_, _, _, true) => Some(Dir::East),
        _ => None,
    }
}

fn handle_window_drag(
    ui: &mut egui::Ui,
    resize: &mut Option<ResizeSession>,
    cursor_screen: &dyn Fn() -> Option<egui::Vec2>,
) {
    // ⚠️⚠️ **必须关掉标签的"可选中"** —— 用户 2026-09-17 报"不能拖动"，根因就在这一行。
    //
    // egui 的 `Label` 在 `interaction.selectable_labels` 打开时（**默认就是打开的**）会往自己的
    // sense 里**加上 `click_and_drag`**（为了让你用鼠标选文字，见 egui `label.rs` 里
    // `Sense::click_and_drag()` 那几行）。而标签画在本函数注册的拖动区**之上** ——
    // 于是**按在任意文字上**（会名 / 状态词 / 计时 / token / 顶栏）都被标签抢走拖拽，
    // 窗口纹丝不动。面板上几乎每个像素都有文字，体感就是"**根本拖不动**"。
    //
    // 挂件是 HUD，不需要选文字；关掉之后标签只剩 hover，拖拽落回窗口拖动区。
    //
    // 放在**这个函数里**而不是调用方：真实链路（`App::ui`）与测试都要经过这里，
    // 写在调用方会出现"测试测不到真实行为"的假绿（第一版就写在了调用方，测试直接红）。
    ui.style_mut().interaction.selectable_labels = false;

    let rect = window_rect(ui);

    // ---- 光标形状：**贴边时必须变成缩放光标** ----
    // 用户 2026-09-17："鼠标放到边缘，光标没有变成可拖动的光标"。
    // 之前压根没设过光标 —— 无边框窗口也没有系统给的那份，所以边缘滑过去毫无提示。
    let hovering = ui.ctx().input(|i| i.pointer.latest_pos());
    if let Some(p) = hovering
        && is_in_resize_band(rect, p)
    {
        let want = match resize_direction_at(rect, p) {
            Some(egui::viewport::ResizeDirection::North) => egui::CursorIcon::ResizeVertical,
            Some(egui::viewport::ResizeDirection::South) => egui::CursorIcon::ResizeVertical,
            Some(egui::viewport::ResizeDirection::East) => egui::CursorIcon::ResizeHorizontal,
            Some(egui::viewport::ResizeDirection::West) => egui::CursorIcon::ResizeHorizontal,
            Some(egui::viewport::ResizeDirection::NorthEast) => egui::CursorIcon::ResizeNeSw,
            Some(egui::viewport::ResizeDirection::SouthWest) => egui::CursorIcon::ResizeNeSw,
            Some(egui::viewport::ResizeDirection::NorthWest) => egui::CursorIcon::ResizeNwSe,
            Some(egui::viewport::ResizeDirection::SouthEast) => egui::CursorIcon::ResizeNwSe,
            None => egui::CursorIcon::Default,
        };
        if want != egui::CursorIcon::Default {
            ui.ctx().set_cursor_icon(want);
        }
    }

    // 覆盖**整个窗口**（不是内容区）。**先注册**（在内容之前），这样将来 M4 加的按钮等
    // 交互控件在命中测试里位于它之上；而标签只 sense hover、不 sense drag，
    // 所以"在任意非控件区域按住即可拖动"成立。
    //
    // ⚠️⚠️ **这里曾经用的是 `ui.max_rect()`（内容区），那是个真 bug**
    // （2026-09-17 用户报"不能随意调整大小"，我实测出来的）：
    // 面板有 8pt 内边距 ⇒ 内容区比窗口小一圈，于是
    //   · 距窗口边 **0~8pt** 的那一圈：`ui.interact` 的矩形不含它 ⇒ **按下毫无反应**；
    //   · 只有 **8~14pt** 那一圈（内容区外扩 6pt 的结果）才会被判成"边缘 → 缩放"。
    // 而人抓边框**手就是往最边上放的** —— 黑边去掉之后更会如此（视觉上的边就是窗口边）。
    // 于是"最边上按下去什么都没发生"，看起来就是不能缩放。
    let response = ui.interact(
        rect,
        egui::Id::new("claude-hud-window-drag"),
        egui::Sense::drag(),
    );

    if response.drag_started() {
        let ctx = ui.ctx();
        // 按**按下点**判方向，而不是当前指针位置 —— 拖动过程中指针早就离开边缘了。
        let dir = ctx
            .input(|i| i.pointer.press_origin())
            .and_then(|p| resize_direction_at(rect, p));
        match dir {
            Some(dir) => {
                // **自己算缩放**，不发给窗口系统（见 `ResizeSession` 的注释）。
                // 拿不到 `viewport()` 的几何时退回 egui 坐标的窗口矩形（原点 0,0）——
                // 测试的无头环境就是这种情况；真实窗口里 `outer_rect` 是**屏幕坐标**，
                // 那是 `OuterPosition` 需要的坐标系，所以就以它为准。
                let (outer, inner) = ctx.input(|i| {
                    (
                        i.viewport().outer_rect.unwrap_or(rect),
                        i.viewport().inner_rect.unwrap_or(rect),
                    )
                });
                // 按下那一刻的光标**屏幕坐标**：优先向系统要（与窗口位置无关，见 `cursor` 模块）。
                // 万一要不到（极少见），退回"窗口原点 + 指针窗口内坐标"——**按下这一刻窗口
                // 确实还没动**，所以这个折算此刻是准的；之后每帧都会重新向系统要。
                let pointer = cursor_screen().unwrap_or_else(|| {
                    outer.min.to_vec2()
                        + ctx.input(|i| i.pointer.press_origin()).unwrap_or_default().to_vec2()
                });
                *resize = Some(ResizeSession { dir, outer, inner, pointer });
            }
            // 从别处起手 → 拖动窗口。这条路走系统命令，**实测是好的**（用户能移动窗口）。
            None => {
                *resize = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
        }
    }

    if let Some(session) = resize.as_mut() {
        if response.dragged() {
            // 拿不到光标就**这一帧什么都不做**（少发一帧命令无害；拿错数会把窗口推着跑）。
            if let Some(c) = cursor_screen() {
                apply_resize(ui.ctx(), session, c);
            }
        } else if response.drag_stopped() || !response.is_pointer_button_down_on() {
            *resize = None;
        }
    }
}

/// 一次手动缩放的起始快照（按下那一刻的窗口几何与指针位置）。
///
/// ## 为什么自己做，不用 `ViewportCommand::BeginResize`
///
/// 那条命令最后落到 winit 的 `drag_resize_window` → `WM_NCLBUTTONDOWN(HT*)`，
/// **把控制权交给系统自己的缩放循环**。问题是它对**无边框窗口**并不可靠：
/// 那个循环靠窗口的"非客户区"来判定可缩放性，而我们的窗口正是把非客户区去掉换来的无边框。
/// 结果就是**命令发出去了、什么都没发生** —— 而且不报错。
/// （本仓第三轮写下这条路时就注明"窗口真的被缩放只能人工验收"，**一直没有人验收过**；
/// 2026-09-17 用户连着两次报"不能缩放"，就是它。）
///
/// 现在改成**每一帧按指针位移自己算**：读当前窗口矩形 + 指针位置，推出新的位置与尺寸，
/// 再发 `OuterPosition` + `InnerSize`。全程在我们手里，而且**能被单测断言**
/// （见 `dragging_the_edge_resizes_the_window_from_that_edge`）。
#[derive(Debug, Clone, Copy)]
struct ResizeSession {
    dir: egui::viewport::ResizeDirection,
    /// 按下那一刻的**外框**（屏幕坐标，egui 点）。
    outer: egui::Rect,
    /// 按下那一刻的**内框**（用来推算"外框比内框大多少"——描边/阴影，这里其实是 0）。
    inner: egui::Rect,
    /// 按下那一刻的指针（**屏幕坐标**）。
    ///
    /// ⚠️ 必须存**屏幕坐标**而不是窗口内坐标：缩放会**移动窗口**（拖左边时原点跟着走），
    /// 窗口内坐标会随之一跳，算出来的位移就是错的。
    ///
    /// ⚠️⚠️ 而且**每一帧的光标位置都要重新向系统要一次**（[`cursor::screen_pos`]），
    /// 不能用 egui 手里那份"窗口内坐标 + 窗口原点"折算 —— 那条路会**正反馈**，
    /// 见 [`apply_resize`] 的注释（2026-09-17 实测把窗口撑到 6582pt 宽后卡死）。
    pointer: egui::Vec2,
}

/// 按当前光标位置算出新的窗口几何并发给窗口系统。**每帧调用一次**（拖动期间）。
///
/// ## ⚠️⚠️ 光标位置为什么必须来自 `GetCursorPos`，而不是 egui 的指针
///
/// 那条路上的量是**混合时刻**的：`窗口原点` 是 egui 现在报的，`指针窗口内坐标` 是它
/// 上一帧收到的。窗口一移动，这两者就**对不上时间**，于是"窗口自己的位移"会被再算一遍
/// 成"鼠标移动了"：
///
/// ```text
/// 估出来的光标屏幕坐标 = 原点(现在) + 指针窗口内坐标(旧) = 真实光标 − 窗口位移
/// ```
///
/// 每帧多推这么一点 → **正反馈，永不收敛**。实测（2026-09-17，自动模拟按住左缘不动）：
/// 窗口 5942 → 6582pt 一路涨、原点跑到 x = −4020、每帧固定 +8pt，而指针**全程没动**；
/// 窗口被撑到几千像素后渲染越来越慢，最后整个程序死掉 —— 用户报的"拖动很慢然后卡死"
/// 就是它。探针还实测到：拖动期间 egui 手里的指针窗口内坐标会**僵在旧值**（`ptr` 全程 = 1
/// 而窗口在跑），所以"换个原点去加"也救不了，只能换数据源。
///
/// `GetCursorPos` 给的是**屏幕坐标**，与窗口在哪毫无关系，谁动都不影响它 ⇒ 回路不存在。
///
/// 拖**右/下缘**时窗口原点本来不动，这条回路不成立 —— 所以缺陷只在**会移动原点的边**
/// （左/上，以及含它们的角）上发作，藏了很久。
fn apply_resize(ctx: &egui::Context, session: &mut ResizeSession, cursor_now: egui::Vec2) {
    use egui::viewport::ResizeDirection as Dir;
    let (outer, inner) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().inner_rect));
    // 与 `handle_window_drag` 里取起点用的是同一套回退，两边必须一致
    let outer = outer.unwrap_or(session.outer);
    let inner = inner.unwrap_or(session.inner);
    let d = cursor_now - session.pointer;

    let mut min = session.outer.min;
    let mut max = session.outer.max;
    if matches!(session.dir, Dir::West | Dir::NorthWest | Dir::SouthWest) {
        min.x += d.x;
    }
    if matches!(session.dir, Dir::East | Dir::NorthEast | Dir::SouthEast) {
        max.x += d.x;
    }
    if matches!(session.dir, Dir::North | Dir::NorthWest | Dir::NorthEast) {
        min.y += d.y;
    }
    if matches!(session.dir, Dir::South | Dir::SouthWest | Dir::SouthEast) {
        max.y += d.y;
    }

    // **最小尺寸**：用户 2026-09-17 要"设置最小的大小限制"。夹住之后要把**被拖的那条边**
    // 顶回去，否则窗口会一边缩一边跑（看起来像在飘）。
    let gap = outer.size() - inner.size(); // 外框比内框大多少（无边框 ⇒ 通常是 0）
    let min_outer = egui::vec2(MIN_INNER_WIDTH, MIN_INNER_HEIGHT) + gap;
    let mut size = max - min;
    // 夹住时**只需要动 `min`**：尺寸已经由 `size` 决定，`max` 是从它推出来的，
    // 再回写一遍是死代码（编译器会报 `value assigned to max is never read` —— 实测报过）。
    // 从**左/上边**拖时，右边/下边是钉住的，所以要把原点往回让；从右/下边拖则原点不动。
    if size.x < min_outer.x {
        size.x = min_outer.x;
        if matches!(session.dir, Dir::West | Dir::NorthWest | Dir::SouthWest) {
            min.x = max.x - size.x;
        }
    }
    if size.y < min_outer.y {
        size.y = min_outer.y;
        if matches!(session.dir, Dir::North | Dir::NorthWest | Dir::NorthEast) {
            min.y = max.y - size.y;
        }
    }

    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(min));
    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size - gap));
}

/// 指针是否落在**缩放带**里（离窗口任一边不超过 `RESIZE_BORDER`）。
///
/// 单独一个函数给光标用：光标要在**还没按下**时就变，而 `resize_direction_at` 只管方向。
fn is_in_resize_band(rect: egui::Rect, p: egui::Pos2) -> bool {
    rect.expand(RESIZE_BORDER).contains(p)
}

/// 状态色。**色相不变，明度整体压暗** —— 这套颜色原是为深灰底挑的（用户抱怨
/// "背景和文字颜色一个颜色"的那一版），换成纯白底之后原值对比度不够：
/// 尤其 `等待确认` 的琥珀 `#e3b341` 在白底上对比度只有 1.7:1，几乎看不见 ——
/// 那正是用户这次要消灭的观感。故按白底重新取值（每种 ≥ 4.5:1，即 WCAG AA 正文标准）。
///
/// 顺序语义不变：`被打断`(#757575) 仍比 `待命`(#4f4f4f) 淡 —— 前者是"已过去的事"，
/// 后者是"还活着的会话"。
fn state_color(s: State) -> egui::Color32 {
    match s {
        State::Working => egui::Color32::from_rgb(0x1e, 0x7a, 0x2e), // 绿，白底 5.4:1
        State::Waiting => egui::Color32::from_rgb(0xa1, 0x62, 0x07), // 琥珀，白底 4.9:1
        State::Error => egui::Color32::from_rgb(0xc0, 0x39, 0x2b),   // 红，白底 5.4:1
        State::Compacting => egui::Color32::from_rgb(0x1f, 0x6f, 0xb2), // 蓝，白底 5.2:1
        State::Done => egui::Color32::from_rgb(0x1f, 0x6f, 0xb2),    // 蓝，同 Compacting
        State::Interrupted => egui::Color32::from_rgb(0x75, 0x75, 0x75), // 灰，白底 4.6:1
        State::Idle => egui::Color32::from_rgb(0x4f, 0x4f, 0x4f),    // 灰，白底 8.3:1
    }
}

/// 行首**色块**的尺寸（pt）。用户 2026-09-16："**色块稍微大一点**"。
///
/// 改之前那根条是**字体里的半块字符 `▍`**（U+258D）—— 想放大就受两样掣肘：
/// 字号一大，行高（= 这一行里最高字块的 max）跟着涨，四段纵向间距全要重标；
/// 而且它**依赖中文字体提供这个字形**（子集里没有）。
/// 现在直接画矩形：尺寸完全由这两个常量决定，与字体、与行高都解耦。
const BAR_WIDTH: f32 = 8.0;
const BAR_HEIGHT: f32 = 20.0; // 行高是 22pt，上下各留 1pt —— 再高就会顶到相邻行
/// 色块圆角。1pt 只是别让它尖得像刀片，不是审美取向。
const BAR_ROUNDING: f32 = 1.0;

/// **会话色板**：行首色块的颜色，用来分辨"这是哪条会话"。
///
/// 用户 2026-09-16 两次裁定："色条区别度不高，每个会话一个颜色" → "**不明显，就用红黄蓝绿**"。
/// 所以这里就是**红·金·蓝·绿**（+紫/石板蓝给第 5、6 条会话，少数情况下才用得上）。
///
/// ⚠️ **"黄"只能是深金**：亮黄在白底上的对比度只有 **1.1:1**，怎么调都过不了 AA，
/// 所以取 `#a16207`（4.92:1）—— 它是这些色相里最接近"黄"、又能满足对比度的那个。
/// 每一色都过白底 WCAG AA（≥ 4.5:1），逐色对比度见 spec §9.0。
///
/// ⚠️ **这四支色相与状态色重合**（红=出错 / 金=等待确认 / 蓝=完成 / 绿=工作中）。
/// 这是用户点名要的效果（明显优先），代价是"红块 ≠ 出错"——**状态由状态词的颜色与文字表示**，
/// 色块只表示身份。改这一块时别把两件事混起来。
///
/// **为什么只有 4 个**：用户原话就是"就用红黄蓝绿"；而且 2026-09-17 实测发现，
/// 原来备用的第 6 色（石板蓝 `#3d4b6b`）在 8×20 的小色块上**与蓝几乎分不出来**
/// （截图取样：`#1565c0` 与 `#3d4b6b` 并排看像同一支）。**色板长度 = 4 也正好等于
/// 用户声明的上限（"我最多开四个"）**，于是"同时可见的互不同色"在他的用法下永远成立。
/// 超过 4 条时从头上重复 —— 已写进 spec。
const SESSION_COLORS: [egui::Color32; 4] = [
    egui::Color32::from_rgb(0xb3, 0x1f, 0x1f), // 红   6.70:1
    egui::Color32::from_rgb(0xa1, 0x62, 0x07), // 金   4.92:1
    egui::Color32::from_rgb(0x15, 0x65, 0xc0), // 蓝   5.75:1
    egui::Color32::from_rgb(0x0f, 0x6b, 0x2e), // 绿   6.64:1
];

/// 一条会话自己的**哈希色**（FNV-1a，确定性 —— 不用 `DefaultHasher`，它每进程随机种子，
/// 重启一次颜色就整体洗牌）。
fn hash_color(session_id: &str) -> egui::Color32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in session_id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    SESSION_COLORS[(h % SESSION_COLORS.len() as u64) as usize]
}

/// 每一行的**期望色下标**（= 它 `session_id` 的哈希色在色板里的位置）。
fn want_of(rows: &[view::Row]) -> Vec<usize> {
    rows.iter()
        .map(|r| {
            let c = hash_color(&r.session_id);
            SESSION_COLORS.iter().position(|x| *x == c).unwrap_or(0)
        })
        .collect()
}

/// 贪心分配 —— **本文件唯一的一套规则**，`assign_colors` 与 `colors_for_display` 共用。
///
/// 给定每行的**期望色下标**，按 `(期望色, session_id)` 定序：先到先得，期望色被占了就取
/// 下标最小的空色，色板全满则重复自己的期望色。返回每行拿到的**色板下标**。
///
/// ⚠️ 定序刻意**不依赖行序**（用 `session_id`，不是行下标）：逐行贪心做不到这点 ——
/// 换个顺序，先来的那条就把色占了，整片跟着变样。列表顺序每次状态变化都会动，颜色不能跟着动。
fn greedy_palette(rows: &[view::Row], want: &[usize]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&a, &b| (want[a], &rows[a].session_id).cmp(&(want[b], &rows[b].session_id)));

    let mut taken = [false; SESSION_COLORS.len()];
    let mut out = vec![0usize; rows.len()];
    for &i in &order {
        let pick = if !taken[want[i]] {
            want[i]
        } else {
            // 期望色被占了：取下标最小的空色；全满（会话数 > 色板）就重复用自己的期望色。
            match taken.iter().position(|t| !t) {
                Some(free) => free,
                None => want[i],
            }
        };
        taken[pick] = true;
        out[i] = pick;
    }
    out
}

/// **全局**分配：对着整份会话列表分配颜色。它要满足的性质只有一条 —— 但这一条正是
/// 整块代码存在的理由：**会话集合不变 ⇒ 颜色一个像素都不变**（不随行序、不随窗高、
/// 不随"这一屏画得下几条"）。
///
/// 为什么必须靠"整份列表"才拿得到：可见集合 = `rows[..fit]`，`fit` 随**窗口高度**变，
/// 成员又随**排序**变 —— 对着它分配，颜色就会在用户什么都没做的时候整体重排
/// （实测：4 条在屏 `[蓝,绿,红,金]`，第 5 条一进来 `5a0c7e13` 红→金、`9d2f6b04` 金→绿）。
/// 而色块**唯一的职责**就是回答"这是哪条会话"，颜色一跳，这件事就答不出来了。
///
/// 代价（用户已知情、spec 里写着"超过 4 条时重复"）：会话数超过色板长度时从头上重复。
/// 可见集合里因此**可能**出现两条同色 —— 这一条由 [`colors_for_display`] 兜底。
fn assign_colors(rows: &[view::Row]) -> Vec<egui::Color32> {
    greedy_palette(rows, &want_of(rows))
        .into_iter()
        .map(|i| SESSION_COLORS[i])
        .collect()
}

/// 真正画的时候用哪几个颜色 = **全局色**，外加"可见集合里撞了色"时的一次局部修复。
///
/// 分工的理由：全局色保证"用户不动手，颜色就不变"，但它对整份列表负责，会话数超过色板
/// 长度时可见的两条可能同色（注释里记着实测截图：不加这一步时第 1 条与第 3 条都是蓝的）。
/// 所以**只在可见集合真的有重复时才重排**，且**只重排可见的那几条** —— 没重复时
/// 原样返回，正是"颜色不跟着窗口大小/排序跑"的那一半。
///
/// ⚠️ 残留代价（据实写在下面，别当成没有）：会话数 > 色板长度时，一旦可见集合里撞了色，
/// 这几条的颜色仍会随"哪几条在屏"而变。要连这一档也钉死，只能让可见的两条同色 ——
/// 那是用户 2026-09-16 明确否掉的（"一眼分得开"），所以这里选了撞色才动。
fn colors_for_display(rows: &[view::Row], global: &[egui::Color32], fit: usize) -> Vec<egui::Color32> {
    let n = rows.len().min(fit);
    let mut out = global.to_vec();
    if n == 0 {
        return out;
    }
    let mut seen = std::collections::HashSet::new();
    let distinct = global[..n]
        .iter()
        .all(|c| seen.insert((c.r(), c.g(), c.b())));
    if distinct {
        return out; // 可见的本来就两两不同 ⇒ 一个像素都不动
    }
    // 期望色取**全局色**（不是哈希色）：这样没撞的那几条会稳稳留在原地，
    // 只有非让位的才换。
    let want: Vec<usize> = global[..n]
        .iter()
        .map(|c| SESSION_COLORS.iter().position(|x| x == c).unwrap_or(0))
        .collect();
    for (i, k) in greedy_palette(&rows[..n], &want).into_iter().enumerate() {
        out[i] = SESSION_COLORS[k];
    }
    out
}

fn state_label(s: State) -> &'static str {
    match s {
        State::Working => "工作中",
        State::Waiting => "等待确认",
        State::Error => "出错",
        State::Compacting => "压缩中",
        State::Done => "完成",
        State::Interrupted => "被打断",
        State::Idle => "待命",
    }
}

/// 「进入当前状态多久」的计时文字。**满 1 小时换单位**：
///
/// ```text
/// 0 .. 59:59      MM:SS   （恒 5 个字符）
/// 1:00 ..         H:MM    （4 个字符起，每跨一个数量级 +1）
/// ```
///
/// 用户 2026-09-21 裁定（遗留 #4 的收口）。改前的式子是 `{:02}:{:02}(分, 秒)`、
/// **分钟不封顶**，于是一个开了 5.8 天的会话右端写着 **`8368:11`** —— 既长（把会名挤掉）
/// 又根本读不懂（那是 139 小时 28 分）。真机上抓到了这一行，才把"计时长度无上界"
/// 这件事从推演变成事实。
///
/// **为什么切换点是 1 小时而不是"MM:SS 撑到 99:59"**：切换那一瞬数字看上去会往回跳，
/// 而 `59:59 → 1:00` 是时钟整点进位、谁都认得；`99:59 → 1:40` 则是纯粹的数字倒流，
/// 看着像 bug。两个方案的**长度收益完全一样**（都会把常见情形封在 6 个字符以内），
/// 所以选可读的那个。
///
/// ⚠️ **上界只是被推远，没有被消灭**：`H:MM` 在 100 小时（4.2 天）后变 6 个字符、
/// 1000 小时（41.7 天）后变 7 个 —— 时钟取消不了，只能让它在真实使用里够不着。
/// [`MIN_INNER_WIDTH`] 那个 222 就是按 **6 个字符**（= 41.7 天以内）定的。
///
/// 秒在 `H:MM` 档**不显示**（`139:28` 不是 `139:28:11`）：这一格是"多久了"的粗读，
/// 分钟级精度足够，而多两位就会把会名再挤掉一格。
fn fmt_elapsed(secs: i64) -> String {
    let mins = secs / 60;
    if mins < 60 {
        format!("{:02}:{:02}", mins, secs % 60)
    } else {
        format!("{}:{:02}", mins / 60, mins % 60)
    }
}

/// 上下文占比那一段的文字。**始终返回非空**（用户裁定"全程显示"，否掉了
/// "只在 >75% 时才显示"的提案）。
///
/// `None`（会话还没有 transcript、读不到 `usage`）时给 `--` 而不是 `0%`：
/// **"读不到"不等于"用了 0%"**，编一个 0 出来会让这一行在关键时候骗人。
/// 比例→百分比用四舍五入到整数（spec §9 的例子：0.68 → `68%`）。
fn fmt_context(pct: Option<f32>) -> String {
    match pct {
        Some(p) => format!("上下文 {:.0}%", p * 100.0),
        None => "上下文 --".to_owned(),
    }
}

/// 上下文占比的颜色：**< warn 用正文黑 / >= warn 用琥珀 / >= danger 用红**
/// （spec §4.6 的阈值口径，阈值取自 `config` 的 `warn_threshold` 50 /
/// `danger_threshold` 75，两个旋钮在本轮之前界面侧没有读者）。
///
/// 两种非黑颜色**直接复用状态色**（`等待确认` 的琥珀、`出错` 的红）—— 它们就是
/// 为纯白底按 WCAG AA（≥4.5:1）挑过的，另起一套只会有第二处要维护的色值。
/// `None` 用正文黑：没有数字可着色。
fn context_color(pct: Option<f32>, cfg: &config::Config) -> egui::Color32 {
    let Some(p) = pct else { return TEXT_COLOR };
    let p100 = p * 100.0;
    if p100 >= cfg.danger_threshold as f32 {
        state_color(State::Error)
    } else if p100 >= cfg.warn_threshold as f32 {
        state_color(State::Waiting)
    } else {
        TEXT_COLOR
    }
}

/// 子代理那一段的文字。**只在 `n > 0` 时被调用** —— 没有子代理时这一段整个不画，
/// 而不是画一个 `子代理 0`（用户要的是"子代理任务情况"，一个恒为 0 的计数是噪音，
/// 也会让"有没有子代理"这个判断在扫视时失效）。
fn fmt_subagents(n: u32) -> String {
    format!("子代理 {}", n)
}

/// 会名截断用的省略号（U+2026）。**数码子集里没有这个字形**（它只有 `0-9` `:`），
/// 由中文/拉丁兜底字体提供 —— 与中文走同一条逐字符回退，见 `text_width` 的注释。
const ELLIPSIS: &str = "…";

/// 会名额度之外再留的余量，防子像素舍入把最后一个像素还回去。
/// 1px 的代价换"计时一定不越界"，划算。
const WIDTH_RESERVE: f32 = 1.0;

/// 用**真实字体栈**量一段文字有多宽（单位 = pt，与 17/15pt 同一坐标系）。
///
/// 走 `Painter::layout_no_wrap` → egui 自己按**逐字符**回退链解析字形，所以：
/// - 数字与 `:` 用的是**数码子集**的宽度（12.24 / 3.0，见 `a_digit_is_measured…`）；
/// - 中文用的是**中文字体**（本机 `msyh.ttc`，微软雅黑）的宽度，**不是**数码的
///   —— 子集里一个 CJK 字形都没有，中文在它那里根本量不出宽度；
/// - 拉丁字母（`web-lab`）用的是**微软雅黑**的宽度。
///
/// 三个方向的宽度差得很多（中文 ≈15、数码数字 12.24、雅黑拉丁又是另一个值），
/// 所以这里**必须**真量，不能拿字符数乘一个常数糊弄。
/// 注：`strong()` 只改颜色不改字体（egui 没有粗体字重）。
fn text_width(ui: &egui::Ui, text: &str, size: f32) -> f32 {
    text_width_in(ui, text, size, egui::FontFamily::Proportional)
}

/// 指定字体族量宽度（[`text_width`] 是它走默认族时的简写）。
///
/// **为什么必须有这个**：会名与顶栏走的是**不含数码子集**的族（见 `TEXT_FAMILY`），
/// 而量宽度若不指定同一个族，就会**按数码宽度算额度、按雅黑渲染** —— 截断点错位。
/// 差别不小，实测同一字号下 `3`：数码 **10.608pt** vs 雅黑 **7.62pt**（差 39%）。
/// 会名里带数字时（`v3 报告`、`2026 Q1`）额度就会明显算歪。
fn text_width_in(ui: &egui::Ui, text: &str, size: f32, family: egui::FontFamily) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), egui::FontId::new(size, family), TEXT_COLOR)
        .size()
        .x
}

/// 走那条**不含数码子集**的族的 `FontId`。顶栏与会名用它（用户 2026-09-15 裁定：
/// "大标题……不用数码"、"会话标题不要用数码的字体"）。
///
/// 计时器**不**用它 —— 那正是数码字体唯一的用武之地。
fn text_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name(TEXT_FAMILY.into()))
}

/// 量一行文字画出来占**多高**（与 [`text_width`] 同一把尺子、同一条字体栈）。
///
/// 存在的唯一理由是 [`paint_right`] 需要一个显式行高 —— 见那里的说明。
/// 单行文本的 galley 高度只由 `FontId` 决定、与内容无关，所以随便给个汉字就够。
fn text_height(ui: &egui::Ui, size: f32, family: egui::FontFamily) -> f32 {
    ui.painter()
        .layout_no_wrap("汉".to_owned(), egui::FontId::new(size, family), TEXT_COLOR)
        .size()
        .y
}

/// 画一「行」：宽度吃满、高度**显式钉死**。
///
/// 不用 `ui.horizontal` 的原因（实测出来的，不是读出来的）：它的高度由内容推出来，
/// 而内容高度取决于混排的字号，于是"这一行占多高"不可预测 —— 同样两个控件，
/// 第二行的块高比它最高的那个 galley 还多 **5pt**（第一行又恰好不多），
/// 于是 `add_space(11)` 量出来是 16pt 而不是 11pt。**行高一旦是隐式的，
/// 任何"间距常量"都不再等于界面上的间距。**
///
/// 行高还**必须**显式给 [`paint_right`] 当居中基准，两处用同一个数。
fn row_line(ui: &mut egui::Ui, row_height: f32, add: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    if !width.is_finite() || width <= 0.0 {
        // ⚠️ 这条兜底**不是**"退化成原来的样子"，而是一种**已知更差**的排布：走的是父面板的
        // 自上而下布局，于是 `▍` 与会名会**竖着**各占一行，而闭包里的 `paint_right` 仍按
        // 面板右缘画、y 还取之前那个 `row_top` —— 两段文字落到同一个 y 上。
        // 它只在窗口窄到 `available_width ≤ 0`（约 22pt）时才可能走到，而
        // `MIN_INNER_WIDTH = 220` 把用户挡在外面。**留着是为了不 panic**，不是为了好看。
        add(ui);
        return;
    }
    ui.allocate_ui_with_layout(
        egui::vec2(width, row_height),
        egui::Layout::left_to_right(egui::Align::Center),
        add,
    );
}

/// 把一段文字贴到**本行右缘**，从右往左排。
///
/// `right_offset` 是"再往左让开多少"（最右那段传 `0.0`），返回下一段该用的新值。
///
/// ⚠️ **这里为什么不用 `ui.with_layout(Layout::right_to_left(..))`**（egui 的惯用法，
/// 而它在别处是好用的）：实测在**本文件这种"上一段是截断过的会名"**的场合，
/// 右对齐布局的**分配**是对的（`min_rect` 正好到内容右界），**绘制**却从
/// `max_rect.right()` 开始**往右**画 —— 于是整组溢出面板，最后一个字形被裁掉半个
/// （实测：220pt 窗宽下 `00:41` 画成 `00:4` + 半个 `1`）。
///
/// 所以改成**自己算位置、直接 `painter.galley`**：右缘 = `max_rect.right()`，
/// 每段再向左让出自己的宽度。落点完全由这里决定，也因此**可以被单测断言** ——
/// 走 `with_layout` 时 `Shape::Text.pos` 报的坐标是错的，测不了。
///
/// 代价（有意取舍）：直接画出来的东西**不进 widget 树**，所以状态词 / 计时 /
/// `子代理 N` 三段不再产生 `Response`，无障碍树里也没有它们（eframe 在 Windows 默认开
/// accesskit）。要拿它们做交互或读屏的话，得回到 `with_layout` 那条老路去解决问题。
///
/// 垂直方向按 `row_height` 居中，与左组 (`Align::Center`) 的基线一致 ——
/// 这一步是**自己算的**，不是 egui 布的局，所以两个方向都得有断言守着
/// （x 见 `the_state_word_and_timer_are_flush_with_the_right_edge`，
/// y 见 `the_right_group_shares_the_left_groups_center_line`）。
fn paint_right(
    ui: &mut egui::Ui,
    text: String,
    size: f32,
    color: egui::Color32,
    row_top: f32,
    row_height: f32,
    right_offset: f32,
) -> (f32, egui::Rect) {
    let galley = ui.painter().layout_no_wrap(
        text,
        egui::FontId::new(size, egui::FontFamily::Proportional),
        color,
    );
    let w = galley.size().x;
    let h = galley.size().y;
    let right = ui.max_rect().right() - right_offset;
    let pos = egui::pos2(right - w, row_top + (row_height - h) / 2.0);
    ui.painter().galley(pos, galley, color);
    // ⚠️ 必须把这一段**自己的框**回给调用方：子代理连线要从"状态词的中点"起笔、
    // 落到"子代理的左缘"，那些坐标只能由**画它的这一处**给出 —— 让调用方拿宽度
    // 自己再算一遍会变成"同一个位置两处各算一遍"（本轮刚因为这个结构出过一个 Critical）。
    (
        w + ui.spacing().item_spacing.x,
        egui::Rect::from_min_size(pos, egui::vec2(w, h)),
    )
}

/// 从「状态词」到「子代理 N」画一条**直角折线**（∟），把"这条会话为什么在跑"指出来。
///
/// 用户 2026-09-16 原话："有子代理在工作中要显示，同时对应的项目工作状态也应为工作中，
/// 这种情况下，我希望可以画一个折线从工作状态指向子代理，一个直角折线，类似于 ∟"。
/// **触发条件（用户同日裁定）：只要有子代理在跑就画** —— 不是"只在该会话因为子代理
/// 而被顶成工作中时才画"。后者会让折线在主代理自己也开始干活时忽隐忽现，
/// 而"有子代理在干活"这个事实没变。
///
/// ```text
/// ▍web-lab                 工作中   02:41
///                                  │
/// 上下文 91%              └────┤ 子代理 2
/// ```
///
/// **几何**（两段都从 `paint_right` 带回来的框上取，不在别处重算一遍）：
/// - 竖段：x = 状态词的**水平中点**；上端 = 状态词下缘 **+2pt**，下端 = 子代理的**垂直中点**。
/// - 横段：y = 子代理垂直中点；右端 = 子代理左缘 **−2pt**，左端接竖段下端。
/// - 那两个 2pt 是**留白**，不是审美：贴着画会与字形相碰（`galley` 的框不含字形的左右边距）。
///
/// 颜色取**状态色**（与「工作中」同一支），肉眼即知这条线说的是它。
///
/// 为什么自己画线段而不是用布局：egui 没有"折线"原语，且**本文件已经定过规矩** ——
/// 右组的落点全部由 `paint_right` 自己算（见那里的注释），因为布局给出的坐标
/// 既要不到、也不可靠。自己算还有个好处：**端点可以被单测断言**。
fn draw_subagent_link(ui: &egui::Ui, state_rect: egui::Rect, sub_rect: egui::Rect, color: egui::Color32) {
    const GAP: f32 = 2.0;
    let stroke = egui::Stroke::new(1.0, color);
    let x = state_rect.center().x;
    let y_top = state_rect.bottom() + GAP;
    let y_mid = sub_rect.center().y;
    let x_end = sub_rect.left() - GAP;

    // 竖段：只有高度为正才画（行高异常时宁可不画，也不画出一条反向的线）。
    if y_mid > y_top {
        ui.painter()
            .line_segment([egui::pos2(x, y_top), egui::pos2(x, y_mid)], stroke);
    }
    ui.painter()
        .line_segment([egui::pos2(x, y_mid), egui::pos2(x_end, y_mid)], stroke);
}

/// 会名走的那条族（`text_width_in` 与绘制两边**必须**用同一个，见 `text_width_in`）。
fn name_family() -> egui::FontFamily {
    egui::FontFamily::Name(TEXT_FAMILY.into())
}

/// 按**字符**（不是字节）把 `name` 截到 `budget` 宽以内，超出部分用 `…` 收尾。
/// 放得下就**原样返回**（不加多余的省略号）。
///
/// 为什么不按字节切：中文一个字 3 个 UTF-8 字节，`&name[..n]` 落在字符中间会
/// **panic**（Rust 的字符串切片是字节索引）；就算不 panic 也会切出半个字。
/// 这里全程走 `chars()`，构造上就不可能切碎。
///
/// `measure` 由调用方注入（真界面传 `text_width`，单测传一把假尺子）：
/// 这样"截断逻辑"与"字体栈怎么量宽"解耦，两边都能单独钉死。
fn truncate_to_width(name: &str, budget: f32, measure: impl Fn(&str) -> f32) -> String {
    if measure(name) <= budget {
        return name.to_owned();
    }
    // 从尾部逐个字符地吐，直到"留下来的前缀 + …"放得下。
    // 逐个字符（而不是二分）是为了**极简可证**：循环不变量就是"当前 chars 是
    // 原名的前缀"，退出时得到的必然是最大可行前缀。
    let mut chars: Vec<char> = name.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate: String = chars.iter().collect::<String>() + ELLIPSIS;
        if measure(&candidate) <= budget {
            return candidate;
        }
    }
    // 连"…"都放不下：宁可这一格空着，也不能把状态词与计时挤出去。
    String::new()
}

/// 这一行**实际要画**的状态词与它的颜色。**只允许有这一个来源。**
///
/// 为什么必须收成一个函数（2026-09-16 复审抓出的 Critical）：`name_budget` 按状态词
/// 扣额度、`draw_row` 按它画字 —— 这两处原来是**各算一遍**的表达式，于是宿主退出时
/// 分叉：额度按 `state_label`（2 字，30pt）扣，画出来的却是 `EXITED_LABEL`（3 字，45pt）。
/// 右组是**画**上去的（不参与布局、不会把会名挤走），少留的 15pt 只能靠余量兜 ——
/// **实测会名叠在「已退出」上**（220pt 宽 + 拉丁长名：会名右缘 109.09 vs「已退出」左缘
/// 104.03，压 5.06pt；默认 320pt 下同样压 2.28pt）。
///
/// 这正是本项目反复出事的结构："两处各算一遍、只有一处改"。
fn state_text(row: &view::Row) -> (&'static str, egui::Color32) {
    if row.host_gone {
        (EXITED_LABEL, EXITED_COLOR)
    } else {
        (state_label(row.state), state_color(row.state))
    }
}

/// 会名这一格能用多少宽度 = 整行可用宽度 − 状态色块 − 状态词 − 计时 − 3 个间隙。
///
/// **状态词与计时是"必须始终完整可见"的两样，所以先扣它们、剩下的才给会名。**
///
/// 扣 **3** 个间隙是**第三轮之前的**排布所需的数：那时四个控件都在流里，
/// `Layout::advance_after_rects` 在**每个控件之后**加一次 `item_spacing.x`
/// （`cursor.min.x = widget_rect.max.x + item_spacing.x`）、行首不加，正好 3 个。
///
/// ⚠️ 现在状态词与计时由 [`paint_right`] **直接画**、不在流里，所以流里只剩
/// `▍`→会名 **1** 个间隙 —— 这条式子**比需要的多扣了 2 个间距**。这是**保守方向**
/// （2026-09-16 复审核对过：最坏情况下会名右缘与右组左缘仍留 `item_spacing.x + WIDTH_RESERVE`
/// = **9pt**），不是缺陷，但别把它当成"精确值"：想收紧额度让会名多显示两个字的话，
/// 先补上 host_gone 那条路径的断言（见 `a_long_name_never_runs_into_the_right_group_...`）。
///
/// 宽度每帧现算（`ui.available_width()`），所以**窗口拉宽 → 额度变大 → 名字显示更长**，
/// 没有任何写死的宽度。
fn name_budget(ui: &egui::Ui, row: &view::Row) -> f32 {
    let spacing = ui.spacing().item_spacing.x;
    // ⚠️ 会名右边现在多了**两样**要扣：今日 token 数（本轮新增）与状态词+计时。
    // 少扣一样，会名就会压到它上面 —— 而右组是 `paint_right` **画**上去的、不会把会名挤走，
    // 所以"算的宽度"与"画的宽度"必须一处不差（这条本轮已经出过一次 Critical）。
    let fixed = BAR_WIDTH
        + text_width(ui, state_text(row).0, STATE_SIZE)
        + text_width(ui, &fmt_elapsed(row.elapsed_secs), STATE_SIZE)
        + 3.0 * spacing;
    ui.available_width() - fixed - WIDTH_RESERVE
}

/// 这一行实际要画的会名：放得下就是原名，放不下就是截断到额度的版本。
fn row_name(ui: &egui::Ui, row: &view::Row) -> String {
    let budget = name_budget(ui, row);
    if !budget.is_finite() {
        // 量不出可用宽度（理论上不可换行的横向布局不会这样，但不赌）：**不截断**。
        // 宁可名字长一点，也不在宽度信息不可信的时候凭猜测砍字。
        return row.name.clone();
    }
    // ⚠️ 量宽度**必须用会名实际渲染的那条族**（`name_family`）：会名不走数码体，
    // 而数码数字比雅黑宽 39%（同字号 10.608 vs 7.62），拿错了额度就会算歪、截断点错位。
    truncate_to_width(&row.name, budget, |s| {
        text_width_in(ui, s, NAME_SIZE, name_family())
    })
}

/// 画一行，**两行文字**（两行各有一条**右缘基准**，用户 2026-09-15 第三轮裁定）：
///
/// ```text
/// ▍ 会名（17pt 加粗黑，过长截断加 …）        工作中   02:13   ← 右组贴右缘
/// 上下文 68%  3.2M                    子代理 2                ← 11.25pt；子代理只在 >0 时画
/// ─────────────────────────────────────────
///                    ↑ 第二行离上面那行 14pt、离下面那条线 8pt（上大下小 = 第二行属于上面那行）
/// ```
///
/// 逐段细节见 `specs/…design.md` §9（权威口径）；四段纵向间距由
/// `the_four_vertical_gaps_are_the_ruled_ones` 逐段守着。
///
/// 为什么分两行而不是挤在一行：第一行那三样是"一眼要看见"的主信息，第二行是
/// 补充。**真帧上量过**（2026-09-22，第二行 11.25pt）：默认 320pt 宽、内容区 298pt 时，
/// 第一行留给会名的额度 ≈ **151pt**（最坏组合下画出来是 `示例方法论系统用…` = 149.84pt），
/// 而第二行那两段 `上下文 68%` = 65.44pt、`子代理 2` = 46.25pt，再加 2 个间隙 16pt
/// = 127.7pt —— 塞进第一行的话会名额度只剩 **23pt**，连两个字都保不住
/// （`MIN_INNER_WIDTH` 存在的理由正是"窄窗下会名至少还剩两个字"）。
/// 所以分行仍然是唯一可行解。代价是每行高度翻倍，但**是常数**（固定两行），
/// 不随子代理数量增长。
///
/// ⚠️ 这段的老数字（`上下文 68%` = 87.25 / `子代理 2` ≈ 62 / 额度 −14）**别再引用**：
/// 那是 `CONTEXT_SIZE` 还是 15pt 的年代量的，与 7.5pt、11.25pt 两代都对不上。
///
/// 单独抽出来是为了让"一行到底画了哪些字"可被单测断到（见
/// `a_row_draws_name_state_timer_and_always_the_context_line`）。
///
/// 会名的截断见 `row_name`：用户顾虑"长会名把计时挤出窗口"，
/// 裁定**加省略号**，且"计时与状态词必须始终完整可见"。
fn draw_row(ui: &mut egui::Ui, row: &view::Row, ledger: &usage::LedgerView, bar_color: egui::Color32, cfg: &config::Config) {
    // 两行的行高各自现算 —— `paint_right` 要它当居中基准。
    //
    // 第一行取**这一行里每一个字块的最高者**。三个都要算进来，别只取会名与状态词：
    // `▍` 是 17pt 比例族（实测高 19），今天 `max` 恰好由会名族 17pt（22）兜住 ——
    // 但那**依赖"会名族首位是微软雅黑"这一条**，没有任何注释或断言守着。
    // 行高一旦小于某个子项，`allocate_ui_with_layout` 的推进量仍是 `row_height`，
    // 内容**不会**把下一行推走，只会静默压到下一行上。
    //
    // （每行每帧量三次 `layout_no_wrap`：这些 job 相同，走 egui 的 layout cache，
    // 首帧之后是哈希命中 —— 不值得为它把行高提到 `draw_panel` 再往下传。）
    let name_row_h = text_height(ui, NAME_SIZE, name_family())
        .max(text_height(ui, NAME_SIZE, egui::FontFamily::Proportional))
        .max(text_height(ui, STATE_SIZE, egui::FontFamily::Proportional));
    // 第二行同理：行高取**这一行里最高的那个字块**。两格现在同号字（11.25pt，`max`
    // 是恒等的），但**别把这句删掉当成常数**：字号一分开，超出行高的那截会**静默压到
    // 下一行上**（`row_line` 的推进量恒等于传进去的行高，内容撑不大它）——
    // 当年 token 那格单独是 15pt 时就是这么坏的：少了这一句，
    // "会话间距 58pt" 与"第一行→第二行 14pt"两条断言立刻红。
    let ctx_row_h = text_height(ui, CONTEXT_SIZE, egui::FontFamily::Proportional)
        .max(text_height(ui, TOKENS_SIZE, egui::FontFamily::Proportional));

    // 右组的落点要自己算，所以行顶 y 得先拿到（`row_line` 会把这一行恰好放在游标处）。
    let top = ui.cursor().min.y;

    // 这两段文字的框由**画它们的那一处**带出来（闭包只能返回值给 `row_line`，
    // 用 `Option` 捕获带回）。子代理连线要用它们的中点与左缘 —— 见 `draw_subagent_link`。
    let mut state_rect: Option<egui::Rect> = None;
    let mut sub_rect: Option<egui::Rect> = None;
    // 这条会话**今天**用了多少 token（用户 2026-09-16 需求 A 的落点：行尾那个数）。
    // 口径 = 该会话自己的今日量，**不是**"它所属项目"的 —— 实测 79/163 份转录跨多个
    // `cwd`，"所属项目"没有唯一解（详见 `usage` 模块头与计划 §4.1）。
    let tokens = ledger.session_tokens(&row.session_id);

    row_line(ui, name_row_h, |ui| {
        // 先量后画（必须在放任何控件之前，否则 available_width 已经被前面的控件吃掉了）。
        let name = row_name(ui, row);

        // 行首色块 = **这条会话的**颜色（用户 2026-09-16："每个会话一个颜色" → "就用红黄蓝绿"）。
        //
        // 改之前它走 `state_color` ⇒ 所有"工作中"的会话都是同一根绿条，扫视分不出谁是谁。
        // 现在：**色块答"这是哪条会话"，状态词答"它怎么了"**（状态词仍按状态着色）。
        //
        // 画的是**矩形**而不是 `▍` 字形（用户："色块稍微大一点"）：字形一大，行高跟着涨、
        // 四段间距全要重标；矩形则由 `BAR_WIDTH`/`BAR_HEIGHT` 说了算，与字体和行高都解耦。
        let (slot, _) = ui.allocate_exact_size(egui::vec2(BAR_WIDTH, name_row_h), egui::Sense::hover());
        ui.painter().rect_filled(
            egui::Rect::from_center_size(slot.center(), egui::vec2(BAR_WIDTH, BAR_HEIGHT)),
            BAR_ROUNDING,
            bar_color,
        );
        ui.label(
            egui::RichText::new(name)
                .strong()
                // 会名**不走数码体**（用户 2026-09-15："会话标题不要用数码的字体，
                // 现在里面的数字还是数码格式"）—— 名字里的数字（`v3`、`2026`）与
                // 其余字符同字体。见 `TEXT_FAMILY`。
                .font(text_font(NAME_SIZE))
                .color(TEXT_COLOR),
        );
        // 宿主没了就压过一切：见 `EXITED_LABEL` 的注释。
        let (label, color) = state_text(row);
        // **状态词与计时成右组贴右缘**（用户 2026-09-15 第三轮裁定）。改前它俩
        // 紧跟在会名后面，于是窗口一拉宽，右边半张面板就是空的（实测 520pt 宽时
        // 第一行只用到 50%）。右组也顺带把"状态与计时必须完整可见"从"不裁"
        // 升级成"有固定位置"。
        //
        // 从右往左：先计时（最右，`offset 0`），再状态词。见 `paint_right`。
        let (off, _timer) = paint_right(
            ui,
            fmt_elapsed(row.elapsed_secs),
            STATE_SIZE,
            TEXT_COLOR,
            top,
            name_row_h,
            0.0,
        );
        state_rect = Some(paint_right(ui, label.to_owned(), STATE_SIZE, color, top, name_row_h, off).1);
    });

    ui.add_space(ROW_LINE_GAP);

    let ctx_top = ui.cursor().min.y;

    // 第二行：上下文占比（**始终画**）+ 子代理（**有才画**，且**右对齐**）。
    //
    // **不缩进**（用户 2026-09-15 第三轮裁定）。此处原本给的理由是"缩进 25pt 会把
    // 三位的百分比 + 两位的子代理数在 200pt 窄窗里挤出右界" —— **那条理由已经过期**：
    // 它成立于 `CONTEXT_SIZE` 还是 15pt 的年代，字号减半之后就失效了。
    //
    // ⚠️ 但**余量在 2026-09-22 之后重新变紧**，别再照抄"还剩一大截"那版说法。
    // 真帧实测（11.25pt，窗宽 = 下限 222，最坏组合 `上下文 100%` + `165.0M` + `子代理 12`）：
    // 左段右缘 **141.03pt**、右段左缘 **158.56pt** ⇒ 中间只剩 **17.53pt**。
    // 也就是说：**当年想缩进的 18pt 现在正好会把它顶破** —— 好在顶格是用户既有的取舍，
    // 而下限挡着用户也到不了 200pt（那一档实测会撞上）。
    // 保持顶格仍然是**取舍**，不是"宽度不够，所以不能缩进"。
    // 这条余量由 `the_token_number_never_runs_into_the_subagent_count` 守着。
    row_line(ui, ctx_row_h, |ui| {
        ui.label(
            egui::RichText::new(fmt_context(row.context_pct))
                .size(CONTEXT_SIZE)
                .color(context_color(row.context_pct, cfg)),
        );
        // **今日 token 放第二行**（在 `上下文 N%` 后面）。
        //
        // ⚠️ 为什么不在第一行（那是用户 2026-09-16 从 mockup 里选的位置）：**会名会没。**
        // 实测（最坏情况：长会名 + `等待确认` + `59:59` + `子代理 12` + `165.0M`）——
        // 放第一行时 **160~240pt 会名整个消失、默认 320pt 只剩 3 个字**；
        // 它会从会名的额度里吃掉约 48pt，而"窄窗下会名至少还剩两个字"是本轮之前
        // 专门调过的不变量（`MIN_INNER_WIDTH` 取 220 的理由就是它）。
        // 第二行左段本来是空的（`上下文 N%` 实测 65.44pt，后面到 `子代理 N` 还有余量），
        // 放这里**不挤会名**。（11.25pt 之后这段余量在最窄窗只剩 17.53pt，见上一段的账。）
        ui.label(
            egui::RichText::new(usage::fmt_tokens(tokens))
                .size(TOKENS_SIZE)
                .color(TOKENS_COLOR),
        );
        if row.subagent_count > 0 {
            sub_rect = Some(
                paint_right(
                    ui,
                    fmt_subagents(row.subagent_count),
                    CONTEXT_SIZE,
                    TEXT_COLOR,
                    ctx_top,
                    ctx_row_h,
                    0.0,
                )
                .1,
            );
        }
    });

    // 子代理连线：见 `draw_subagent_link` 的说明（用户 2026-09-16 裁定：有子代理就画）。
    if let (Some(sr), Some(br)) = (state_rect, sub_rect) {
        draw_subagent_link(ui, sr, br, state_text(row).1);
    }

    ui.add_space(ROW_BOTTOM_GAP);
    rule(ui);
    // 会话之间再补一段空白 —— 用户裁定"间距扩大一倍"，依据见 `EXTRA_ROW_GAP`。
    ui.add_space(EXTRA_ROW_GAP);
}

/// 面板正文：顶栏 + 每行的「会名 + 状态 + 计时」/「上下文 + 子代理」两行。
///
/// 抽出来是为了让无头测试能跑到**同一份**渲染代码（`draw_panel` 由 `App::ui` 与
/// `run_frame` 共用），否则测试里那份复制品会与真界面悄悄分叉。
///
/// 第二轮变化：**上下文占比加回来了，而且始终显示**（用户否掉了"只在 >75% 时才
/// 显示"的提案）；`detail`（当前工具）/ `progress`（步数）/ `subtitle`（摘要）三个
/// 分支仍然**没有**（字段仍在 `view::Row` 里、仍被赋值，见那边的注释）。
///
/// `cfg` 只用来读 `warn_threshold` / `danger_threshold` —— 这两个旋钮此前在界面侧
/// **没有任何读者**（`config.rs` 的注释写着"保留待 M4"），本轮的上下文配色正是它们
/// 该有的读者。
/// `first_scan_done` = **后台第一轮扫描是否已经落地**（见 [`SnapshotData`]）。为 `false`
/// 时"0 个"与"没有活跃会话"**都不说** —— 那两句话此刻是错的。带这个参数的只有
/// `App::ui`（传 `snap.first_scan_done`），测试直接调 `draw_panel` 时一律传 `true`：
/// 测试造出来的那批 `rows` **就是**"已经扫到的结果"，没有"还没到"这回事。
fn draw_panel(
    ui: &mut egui::Ui,
    rows: &[view::Row],
    ledger: &usage::LedgerView,
    first_scan_done: bool,
    cfg: &config::Config,
) {
    // 纵向空隙**全部显式给**（每处 `add_space`），把 egui 的隐式 `item_spacing.y` 清零。
    //
    // 不清零的后果（实测过）：`add_space(11)` 实际只加了 11，但"第一行→第二行"的
    // 落点是 **8 + 11 = 19** —— 那个 8 是隐式的、散在三处（item_spacing、horizontal
    // 块高、separator 占位）。于是常量名承诺的数字与界面上量到的数字对不上，
    // 下一个改数值的人得先反推一遍隐式量。这一轮就是这么量错的。
    //
    // 只用 `.y`：横向的 `item_spacing.x`（= 8）仍要给 `name_budget` 用。
    ui.spacing_mut().item_spacing.y = 0.0;

    // 顶栏：会话数 + **全机今日 token 总量**（用户 2026-09-16 需求 B）。
    //
    // 有**两样**都可能是"还不知道"：账本扫过了没有（`ledger.day()`）、以及后台第一轮
    // 扫描落地了没有（`first_scan_done`）。两者都按同一条规矩处理 ——
    // **读不到就不写那个数**，而不是写个 0。
    //
    // 会话数原先只认 `first_scan_done` 之外什么都不认，于是启动头一秒显示
    // "● claude 会话 · **0 个**"，一秒后跳成真实条数（遗留 #6）。
    let mut header = "● claude 会话".to_owned();
    if first_scan_done {
        header.push_str(&format!(" · {} 个", rows.len()));
    }
    if ledger.day().is_some() {
        header.push_str(&format!(" · 全部 {}", usage::fmt_tokens(ledger.total)));
    }
    ui.label(
        egui::RichText::new(header)
            .color(TEXT_COLOR)
            // 顶栏**统一不走数码体**（用户 2026-09-15 裁定）：走那条不含子集的族，
            // 于是里面的 `3` 与其余字符同字体。见 `TEXT_FAMILY`。
            .font(text_font(HEADER_SIZE)),
    );
    rule(ui);
    // 顶栏与正文之间留一段明确空白（用户 2026-09-15 第三轮裁定 16pt）——
    // 改前只有 6pt，比会话内部两行之间的 8pt 还紧，层级是倒的。见 `HEADER_GAP`。
    ui.add_space(HEADER_GAP);

    if rows.is_empty() {
        // ⚠️ **首轮扫描还没落地时不能画这句**：那一刻"空"的意思是"还不知道"，
        // 不是"确实没有"（启动头一秒亮一句"没有活跃会话"再跳成好几条，是遗留 #6）。
        if first_scan_done {
            ui.label(
                egui::RichText::new("没有活跃会话")
                    .color(egui::Color32::from_rgb(0x6a, 0x6a, 0x6a))
                    .size(STATE_SIZE),
            );
        }
        return;
    }
    // 一条会话**自己的内容**要多高 = 第一行 + `ROW_LINE_GAP` + 第二行。
    //
    // ⚠️ 刻意**不含 `ROW_BOTTOM_GAP` 与那条分隔线**：它们属于"这一行**之后**的空白与装饰"
    // （`EXTRA_ROW_GAP` 同理），最后一条会话下面放不放得下那段空白，与"这条会话看得清不清"
    // 毫无关系。第一版把它们算进来了，结果**默认 320×360 下第 4 条会话被整个吞掉**
    // （判它"装不下"，而它的文字其实离底边还有 4pt）—— 用户当天就发现了："四个会话为什么只显示了 3 个"。
    let one_row_h = text_height(ui, NAME_SIZE, name_family())
        .max(text_height(ui, NAME_SIZE, egui::FontFamily::Proportional))
        .max(text_height(ui, STATE_SIZE, egui::FontFamily::Proportional))
        + ROW_LINE_GAP
        + text_height(ui, CONTEXT_SIZE, egui::FontFamily::Proportional);
    // 先算出**这一屏真正画得下几条**（`fit`），颜色交给 `colors_for_display`：
    // 它拿**整份列表**的全局色，只在"可见的这几条撞了色"时才局部重排。
    //
    // ⚠️ 曾经直接对着 `rows[..fit]` 分配 —— 那样可见的必然两两不同色，但**颜色会跟着
    // 窗口高度和排序跑**（用户什么都没做，已上屏的会话就换了色；实测 4 条在屏
    // `[蓝,绿,红,金]`，第 5 条一进来 `5a0c7e13` 红→金、`9d2f6b04` 金→绿）。
    // 也不能**只**对着整份列表分配：会话数超过色板长度时可见的两条会撞色
    // （实测截图：第 1 条与第 3 条都是蓝的）。两个都躲开 = 两步走，见那两个函数。
    let start_y = ui.cursor().min.y;
    let bottom = ui.max_rect().bottom();
    // ⚠️ 这里**不是**除以 `one_row_h` —— 相邻两条会话的前进量是
    // `one_row_h + ROW_BOTTOM_GAP + EXTRA_ROW_GAP`（行尾空白 + 会话间空白）。
    // 第一版只除 `one_row_h`，算出"装得下 7 条"、实际只画 4 条（实测 start_y=41、
    // bottom=372、one_row_h=44 ⇒ 7；而每行前进 94pt ⇒ 真装 4 条），于是颜色是
    // **对着 7 条分配的**、看得见的 4 条里照样撞色。
    let row_advance = one_row_h + ROW_BOTTOM_GAP + EXTRA_ROW_GAP;
    let fit = if bottom - start_y < one_row_h {
        0
    } else {
        (((bottom - start_y - one_row_h) / row_advance).floor() + 1.0) as usize
    };
    let colors = colors_for_display(&rows, &assign_colors(&rows), fit);
    for (i, row) in rows.iter().enumerate() {
        // **装不下就整个不画，不画半条。**（用户 2026-09-17："里面的文字排版自动跟随大小变化"）
        //
        // 改之前是硬画下去、由窗口去裁：窗口拖矮之后，最后一条会话会只剩个名字挂在底边，
        // 第二行（上下文/子代理）被切掉 —— 看着像界面坏了，而不是"这里放不下"。
        // 判据拿 `one_row_h` **整条**去比，所以不会出现"半个名字算放得下"的情况。
        if ui.cursor().min.y + one_row_h > ui.max_rect().bottom() {
            break;
        }
        draw_row(ui, row, ledger, colors.get(i).copied().unwrap_or_else(|| hash_color(&row.session_id)), cfg);
    }
}

/// 面板外观：**纯白底 + 粗黑描边**（用户裁定的小游戏 HUD 框感）。
///
/// 抽成函数是为了可测 —— 白底黑边的**观感**单测覆盖不了（要人眼看），但"填的是
/// 纯白、描边是纯黑、够粗"这三条是可以断言的事实，而且它们正是"改成白底黑边"
/// 这句话里唯一能自动核对的部分。
///
/// `corner_radius` 保持 `NONE`：小游戏 HUD 的框是**直角**的，圆角会把框感削掉。
/// 面板四角的圆角半径（pt）。
///
/// 用户 2026-09-16 裁定："**挂件改为圆角**"。
///
/// ⚠️ **它需要一个前提：窗口必须是透明的**（见 `run` 里 `with_transparent(true)` 与
/// `App::clear_color`）。面板本身**仍然是纯白不透明的**（`PANEL_FILL` 的 alpha = 255）——
/// 透明只发生在**圆角切掉的那四块小三角**上，桌面从那里透出来，看上去才是圆的。
/// 不透明窗口里画圆角，四角只会露出窗口自己的底色（一块方角），等于没做。
///
/// 10pt 是"看得出是圆的、又不吃掉内容"的折中：描边 3pt、内边距 8pt，
/// 半径再大就会把第一行的色块挤得离边框太近。
const PANEL_ROUNDING: f32 = 10.0;

/// 面板外观：**纯白底 + 粗黑描边 + 圆角**（用户裁定的"小游戏 HUD 框感"）。
fn panel_frame() -> egui::Frame {
    egui::Frame::NONE
        .fill(PANEL_FILL)
        // 没有描边（用户 2026-09-16 去掉黑边）：面板就是一块**纯白圆角**。
        .corner_radius(PANEL_ROUNDING)
        .inner_margin(egui::Margin::same(INNER_MARGIN))
}

struct App {
    cfg: config::Config,
    /// 只读快照，由后台线程写。渲染线程**不做任何 IO**。
    snapshot: Snapshot,
    alive: Arc<AtomicBool>,
    /// 退出时 join（睡眠是切片式的，最多等 50ms），确保 `request_repaint`
    /// 不会打到已经关闭的窗口上。
    worker: Option<std::thread::JoinHandle<()>>,

    // ---- 窗口位置记忆（见 `should_save_pos`）----
    /// 最近一帧观察到的窗口位置（用来判断"停稳了没有"）。
    last_pos: Option<egui::Pos2>,
    /// 位置**连续不变**是从什么时候开始的（秒，`ctx.input().time`）。
    pos_stable_since: Option<f64>,
    /// 已经写进配置的位置 —— 用来判断"真的动了没有"，避免重复写盘。
    saved_pos: [f32; 2],

    /// 正在进行的**手动缩放**（见 `ResizeSession`）。`None` = 没在缩。
    ///
    /// 状态必须跨帧存活：按下那一帧只记录起点，之后每一帧都要拿它换算新几何。
    resizing: Option<ResizeSession>,
}

impl App {
    /// 记住窗口位置。**每帧调用**，但只有"停稳且真的动了"时才写盘。
    ///
    /// 位置从 `viewport().outer_rect` 取（外框原点）—— 无边框窗口的 outer 与 inner
    /// 只在有系统装饰时才有差别，但用 outer 与 `ViewportBuilder::with_position` 的语义一致
    /// （那个收的也是外框位置）。
    fn remember_window_pos(&mut self, ctx: &egui::Context) {
        let (cur, now) = ctx.input(|i| (i.viewport().outer_rect.map(|r| r.min), i.time));
        let Some(cur) = cur else {
            return; // 平台拿不到位置（或窗口还没建好）：这一帧不记
        };
        if self.last_pos != Some(cur) {
            self.last_pos = Some(cur);
            self.pos_stable_since = Some(now);
            return;
        }
        let stable_for = now - self.pos_stable_since.unwrap_or(now);
        let cur_arr = [cur.x, cur.y];
        if !should_save_pos(cur_arr, self.saved_pos, stable_for) {
            return;
        }
        self.cfg.window_pos = cur_arr;
        // 写失败**不打扰用户**：位置记忆是锦上添花，失败只意味着"下次回默认位置"。
        // （配置读坏是另一回事——那条路径有对话框，见 `config.rs` 的存档逻辑。）
        if config::save_to(&paths::real_config_path(), &self.cfg).is_ok() {
            self.saved_pos = cur_arr;
        }
    }
}

impl eframe::App for App {
    /// 窗口底色**全透明** —— 圆角切掉的那四个角靠它才透得出桌面。
    ///
    /// 面板自身的填充由 `panel_frame()` 决定（纯白、alpha 255），所以**面板依然不透明**；
    /// 这里透明的是"面板没画到的地方"。两个一起改才对：只开窗口透明、不给这个 clear color，
    /// 四角会被窗口默认底色填满，看起来还是方的。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    // eframe 0.36 的 `App` 必需方法叫 `ui`（收 `&mut Ui`），旧版的
    // `update(&mut self, ctx, frame)` 已不存在 —— brief 的写法对应的是 0.31 一代的 API，
    // 在 Cargo.toml 钉住的 0.36.2 上无法编译。此处按 0.36 的签名改写，
    // 轮询/重绘请求与面板渲染的语义保持不变（重绘经 `ui.ctx()`，面板经 `show(ui, …)`）。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 只读快照（克隆几行 + 一份账本**视图**，量级是几 KB），IO 已由后台线程做完。
        //
        // ⚠️ 快照里那半本账**必须是 `usage::LedgerView` 而不是整本 `Ledger`**：这句注释
        // 原先写着"几百字节"，那是 token 账本加进来**之前**的话 —— 整本账每帧克隆实测
        // **172 µs**（release，2026-09-18），而挂件在拖动/悬停时按帧重画，白烧。
        // 视图只有 UI 真读的三样，同样的量法 **1.8 µs**（97 倍）。见 `usage::Ledger::view`。
        let snap: SnapshotData = self
            .snapshot
            .lock()
            .map(|r| r.clone())
            .unwrap_or_default();

        // 窗口位置记忆：拖完之后**停稳 1 秒**才写一次配置（判据是纯函数 `should_save_pos`，
        // 单测钉着）。这里是渲染线程里唯一的写盘，且只在"真的动了"时发生。
        self.remember_window_pos(ui.ctx());

        // 时间相关的显示（"00:12" 计时、done→idle 降级）仍需要按 poll_interval_ms 重绘，
        // 与数据新鲜度无关 —— 数据到了后台线程会额外唤醒一次。
        ui.ctx().request_repaint_after(Duration::from_millis(self.cfg.poll_interval_ms));

        egui::CentralPanel::default()
            .frame(panel_frame())
            .show(ui, |ui| {
                handle_window_drag(ui, &mut self.resizing, &cursor::screen_pos);
                draw_panel(ui, &snap.rows, &snap.ledger, snap.first_scan_done, &self.cfg);
            });
    }

    /// 停掉后台轮询线程再让 eframe 拆上下文。睡眠是切片式的，最多等 50 ms。
    fn on_exit(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionState;
    // `Progress` 只在下面的断言里用得到 —— 生产代码已不引用它（界面不再渲染步数）。
    // 从模块顶部搬进来，否则 `cargo build` 会多一条"unused import"。
    use crate::view::Progress;
    use std::fs;

    use crate::testtmp::TempDir;

    fn tempdir(tag: &str) -> TempDir {
        TempDir::new("ui", tag)
    }

    /// 造一个**头部自洽**的假字体。
    ///
    /// 这里刻意不写真字体内容 —— `looks_like_font` 只做头部自洽性检查，不解析表目录
    /// 内容。但**头部必须自洽**：第一版这个辅助只写 `magic + 64 个零字节`，于是所有
    /// "不是字体的字节要被挡掉"的测试都只验到了 4 字节 magic 这一层，`looks_like_font`
    /// 声称挡住"截断文件"而实际没挡的洞，一条测试都发现不了。
    fn fake_font(path: &Path, magic: &[u8; 4]) {
        let mut b = magic.to_vec();
        if magic == b"ttcf" {
            b.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]); // version
            b.extend_from_slice(&1u32.to_be_bytes()); // numFonts = 1
            b.extend_from_slice(&16u32.to_be_bytes()); // 唯一字体的偏移 = 16
            b.resize(28, 0); // 让"偏移 16 + 12 字节 sfnt 头"落在文件内
        } else {
            b.extend_from_slice(&2u16.to_be_bytes()); // numTables = 2
            b.resize(12 + 16 * 2, 0); // 12 字节头 + 2 项表目录
        }
        fs::write(path, b).unwrap();
    }

    // ---- 字体栈的真机验证辅助（`ctx` 要先跑过一帧，字体才建好）------------------
    //
    // ⚠️ 一条踩过的坑，写在这里免得后人重踩：**`has_glyphs` 会假阴性**。
    // egui（epaint）的实现是
    //     `has_glyph(c) = resolve_face(c) != replacement_face_key`
    // 而 `replacement_face_key` = 该族里拥有替换字符（'◻' U+25FB，没有就退到 '?'）
    // 的**那个 face**。于是只要"能画出 c 的 face"恰好也是"替换字形的 face"，
    // 就会返回 false，尽管 c 明明画得出来。真机两条实测：
    //   · 对照数码子集独装：族里只有它，而它有 '?' → 替换 face 就是它自己 →
    //       `has_glyphs("0") == false`（假阴性），但 `width("0") == 12.24`。
    //   · egui 自带等宽族 `[Hack, Ubuntu-Light, NotoEmoji, emoji-icon]`：Hack 自己
    //       有 '◻'（用 fontTools 查过：`uni25FB`）→ 替换 face == Hack →
    //       `has_glyphs("0") == false`（同样的假阴性）。
    //   而比例族 `[Ubuntu-Light, …]` 里 Ubuntu-Light **没有** '◻' → 替换 face 是
    //   NotoEmoji ≠ Ubuntu-Light → 同一串字符返回 true。
    // 结论：**"某个字形由哪个字体提供"不能只靠 `has_glyphs`**，必须同时看
    // advance width（宽度由解析到的那个 face 决定，与替换 face 无关）。
    // 下面的用例都是按这条结论写的。

    /// 取一个已经跑过一帧的 Context（`has_glyphs`/`glyph_width` 要求字体已经建好：
    /// `Context::fonts_mut` 的文档写着 "Not valid until first call to Context::run()"）。
    fn ctx_with_fonts(fonts: egui::FontDefinitions) -> egui::Context {
        let ctx = egui::Context::default();
        ctx.set_fonts(fonts);
        let mut full = ctx.run_ui(egui::RawInput::default(), |_| {});
        full.textures_delta.clear(); // 不 clear 会 panic（见拖拽用例的注释）
        ctx
    }

    fn has_glyphs_in(ctx: &egui::Context, fam: egui::FontFamily, s: &str) -> bool {
        ctx.fonts_mut(|f| f.has_glyphs(&egui::FontId::new(15.0, fam), s))
    }

    /// 发货栈的**两个族都要能画出来**（字体是按族配的，只测一族会漏掉另一族）。
    fn has_glyphs(ctx: &egui::Context, s: &str) -> bool {
        [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
            .iter()
            .all(|fam| has_glyphs_in(ctx, fam.clone(), s))
    }

    /// 单个字符的 advance width —— 用来判定**这个字形实际由哪个字体渲染**
    /// （`has_glyphs` 做不到这件事，见上面那条坑）。
    fn glyph_width(ctx: &egui::Context, c: char) -> f32 {
        ctx.fonts_mut(|f| f.glyph_width(&egui::FontId::proportional(15.0), c))
    }

    /// 只装**数码子集**的 Context：族里只有它（用来拿"数码头自己的宽度"当基准）。
    /// 注意这个 Context 的 `has_glyphs` 全是假阴性（见上面那条坑），只用来量宽度。
    fn ctx_digits_only() -> egui::Context {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            DIGITS_NAME.to_owned(),
            Arc::new(egui::FontData::from_static(DIGITS_BYTES)),
        );
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.insert(fam, vec![DIGITS_NAME.to_owned()]);
        }
        ctx_with_fonts(fonts)
    }

    /// 只装**某一个**字体的 Context（族里只有它）。用来拿"这个字体自己的宽度"
    /// 当基准 —— 下面判定"某字形由谁渲染"全靠宽度比对。
    fn ctx_single_font(name: &str, bytes: Vec<u8>) -> egui::Context {
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.insert(fam, vec![name.to_owned()]);
        }
        ctx_with_fonts(fonts)
    }

    /// 本机 `pick_font` 实际会装上的那个中文字体（本机 = 微软雅黑）。
    /// 一个都取不到时返回 `None` —— 非中文 Windows 上相关用例会**跳过**而不是失败。
    fn local_cjk() -> Option<(PathBuf, Vec<u8>)> {
        pick_font(&font_candidates())
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    /// 用户 2026-09-14 第二轮的**核心断言**："数字（含 `:`）→ 数码；拉丁字母与中文
    /// → 微软雅黑；**混合串里也要成立**"。
    ///
    /// 判据只有一条：**advance width 等于谁的宽度，字形就是谁画的**（宽度由解析到的
    /// 那个 face 决定，与 `has_glyphs` 的替换 face 无关 —— 见上面那条坑）。
    /// 三层对照 Context：只装数码子集 / 只装本机中文字体 / 发货栈。
    #[test]
    fn digits_go_through_the_digital_subset_and_latin_goes_through_the_cjk_font() {
        let Some((font, cjk_bytes)) = local_cjk() else {
            eprintln!("跳过：本机一个中文字体都取不到，雅黑一侧的宽度无从量起");
            return;
        };
        let digits = ctx_digits_only();
        let cjk = ctx_single_font("only-cjk", cjk_bytes.clone());
        let ship = ctx_with_fonts(build_font_definitions(Some(cjk_bytes)));
        eprintln!("本机 pick_font 取到的中文字体 = {font:?}");

        // ① 数字与 `:` → 数码子集的宽度（且与雅黑不同，否则本判据区分不出是谁画的）
        for c in "0123456789:".chars() {
            let (w_ship, w_digits, w_cjk) =
                (glyph_width(&ship, c), glyph_width(&digits, c), glyph_width(&cjk, c));
            assert!(w_digits > 0.0, "数码子集里 {c:?} 竟没有宽度（子集做坏了？）");
            assert!(
                close(w_ship, w_digits),
                "{c:?} 在发货栈里宽 {w_ship}，数码子集里宽 {w_digits} —— 说明它没走数码体"
            );
            assert!(
                !close(w_digits, w_cjk),
                "前提不成立：数码子集与中文字体在 {c:?} 上同宽（{w_digits} vs {w_cjk}），\
                 本用例就区分不出字形来自谁了"
            );
        }

        // ② 拉丁字母与中文 → 中文字体（雅黑）的宽度，**不是**数码的
        for c in "Aa中骑".chars() {
            let (w_ship, w_digits, w_cjk) =
                (glyph_width(&ship, c), glyph_width(&digits, c), glyph_width(&cjk, c));
            assert!(
                close(w_digits, 0.0),
                "数码子集里竟然有 {c:?} —— 子集本该只留 0-9 与 `:`，多留就会把拉丁/中文\
                 也抢成数码风格（这正是本轮要修的问题）"
            );
            assert!(w_cjk > 0.0, "中文字体里 {c:?} 没有宽度");
            assert!(
                close(w_ship, w_cjk),
                "{c:?} 在发货栈里宽 {w_ship}，中文字体里宽 {w_cjk} —— 说明它没走雅黑"
            );
            assert!(!close(w_ship, w_digits), "{c:?} 不该有与数码体相同的宽度（那是 0）");
        }

        // ③ 混合串：整串量宽 = 各段按上面的规则拼出来的宽度。
        //    这正是"混合字符串里也成立"的可执行版本 —— 若 egui 是"整串选一个 face"
        //    （而不是逐字符回退），`02:13` 与 `上下文 68%` 里必然有一段用错字体，
        //    整串宽度就会与逐段手算的和对不上。
        let mixed = "上下文 68%";
        let expect = measure_in_stack(&ship, "上下文 ", 15.0)
            + measure_in_stack(&ship, "68", 15.0)
            + measure_in_stack(&ship, "%", 15.0);
        let got = measure_in_stack(&ship, mixed, 15.0);
        assert!(
            close(got, expect),
            "{mixed:?} 整串宽 {got} ≠ 逐段手算的和 {expect} —— 说明混合串里有字符走错了字体"
        );
        //    并且这条不是恒真：如果整串都走雅黑、或整串都走数码，宽度会不同。
        assert!(
            !close(expect, measure_in_stack(&cjk, mixed, 15.0)),
            "前提不成立：整串走雅黑的宽度与逐段混排相同，本用例就区分不出混排是否生效"
        );
    }

    #[test]
    fn the_digital_font_is_a_strict_subset_of_digits_and_colon() {
        // 子集本身的事实（与机器无关，只查内嵌字节）：
        //   · 只有 `0-9` 与 `:` 有宽度 —— 拉丁/中文/空白一律 0；
        //   · 数字 12.24、`:` 3.0（= 816/1000 与 200/1000 em，@15pt）—— 与原版
        //     DSEG14 逐字相同，这就是"子集没有偷偷挪动数字宽度"的证据。
        let digits = ctx_digits_only();
        for c in "0123456789".chars() {
            assert!(close(glyph_width(&digits, c), 12.24), "{c:?} 的宽度变了");
        }
        assert!(close(glyph_width(&digits, ':'), 3.0), "`:` 的宽度变了");
        for c in "AaZz %.-中".chars() {
            assert!(
                close(glyph_width(&digits, c), 0.0),
                "{c:?} 不该在数码子集里 —— 它会被渲染成数码风格（本轮要修的就是这个）"
            );
        }
    }

    #[test]
    fn the_font_candidate_table_prefers_microsoft_yahei() {
        // 用户点名"其他的用微软雅黑"，而本机的雅黑是 `msyh.ttc`（TTC）。
        // 这里钉的是**表的顺序**（与磁盘无关）：雅黑必须排在等线等其它候选之前。
        let c = font_candidates();
        assert!(
            c[0].ends_with("msyh.ttc"),
            "候选表第一项应当是微软雅黑（msyh.ttc），实际 {:?}",
            c[0]
        );
        assert!(c[1].ends_with("msyh.ttf"), "单文件版雅黑紧随其后：{:?}", c[1]);
        assert!(
            c.iter().position(|p| p.ends_with("Deng.ttf"))
                < c.iter().position(|p| p.ends_with("simsun.ttc")),
            "取不到雅黑时要按序回落，不能把宋体排到等线前面"
        );
    }

    #[test]
    fn the_ttc_actually_loads_and_its_glyphs_are_the_ones_used() {
        // 真机验证"`.ttc` 能不能吃"。上一轮只验过 `Deng.ttf`（单文件），本轮候选表
        // 第一项换成了 `msyh.ttc`（TTC）—— egui 0.36 走 skrifa/harfrust 的
        // `FontRef::from_index(bytes, 0)`，理论上吃得下 TTC 的 index 0，但**必须实测**：
        // 解析失败在 epaint 里是 `panic!`（fonts.rs:990），不是静默回退。
        let Some((path, bytes)) = local_cjk() else {
            eprintln!("跳过：本机一个中文字体都取不到");
            return;
        };
        eprintln!("实际加载的中文字体 = {path:?}（{} 字节）", bytes.len());
        assert!(looks_like_font(&bytes), "挑出来的字节不像字体");

        // 装进 Context 会在这里 panic（如果 skrifa 解析不了这个容器）
        let ship = ctx_with_fonts(build_font_definitions(Some(bytes)));
        // 装得上还不够 —— 还要真的量得出宽度，且不是 0（0 = 一个字形都没解析出来）
        for c in "工作中Aa68".chars() {
            assert!(
                glyph_width(&ship, c) > 0.0,
                "{c:?} 在 {path:?} 里量不出宽度 —— 字体装上了但没有字形"
            );
        }
    }

    #[test]
    fn chinese_can_only_come_from_the_cjk_font_never_from_the_digital_one() {
        // 中文来源的完整证据链。三条缺一不可：
        //   ① 中文**不由**数码子集提供 —— 子集只留 11 个字符，在"只装数码子集"的
        //      Context 里中文的 advance width 是 **0**（没有字形）。这里用宽度而不是
        //      `has_glyphs`：数码体独装时 `has_glyphs` 是假阴性，见上面那条坑。
        //   ② 中文**也不由** egui 自带字体提供 —— `build_font_definitions(None)`
        //      那条栈（[digits, Ubuntu-Light, NotoEmoji, emoji-icon]）对中文的
        //      `has_glyphs` 是 false：一个含 CJK 的 face 都没有。
        //   ③ 于是发货栈里的中文**只可能**来自 `pick_font` 选中的那个中文字体。
        let digits = ctx_digits_only();
        for c in "工作中等待确认被打断".chars() {
            assert_eq!(
                glyph_width(&digits, c),
                0.0,
                "{c:?} 在只装数码子集的 Context 里竟然有宽度 —— 子集不该有 CJK 字形"
            );
        }

        let no_cjk = ctx_with_fonts(build_font_definitions(None));
        for s in ["工作中", "等待确认", "被打断", "待命", "出错", "压缩中", "完成"] {
            assert!(
                !has_glyphs(&no_cjk, s),
                "{s:?} 竟然被画出来了 —— 那说明字体栈里混进了含 CJK 的字体，\
                 中文的来源假设（由中文字体兜底）需要重写"
            );
        }

        // ③ 本机有中文字体时：发货栈画得出，且宽度与数码子集不同（不是它画的）
        let Some((path, bytes)) = local_cjk() else {
            panic!(
                "本机候选表里一个可用的中文字体都没有，中文验证无法进行：{:?}",
                font_candidates()
            );
        };
        let ship = ctx_with_fonts(build_font_definitions(Some(bytes)));
        for s in [
            "工作中",
            "等待确认",
            "被打断",
            "没有活跃会话",
            "● claude 会话 · 3 个", // 顶栏原样：含 ● 与 ·
            "上下文 68%",           // 本轮新加的上下文行原样
            "子代理 2",             // 本轮新加的子代理段原样
            "示例项目报告排版修复",
            "样例项目…", // 会名截断用的省略号（U+2026）同样由中文字体提供
        ] {
            assert!(has_glyphs(&ship, s), "发货栈必须画得出 {s:?}（中文字体：{path:?}）");
        }
        for c in "工作中●…".chars() {
            let (w_ship, w_digits) = (glyph_width(&ship, c), glyph_width(&digits, c));
            assert!(w_ship > 0.0, "{c:?} 在发货栈里没有宽度");
            assert!(
                !close(w_ship, w_digits),
                "{c:?} 的宽度（{w_ship}）与数码子集相同 —— 那它就不是中文字体画的"
            );
        }
    }

    fn state_file(dir: &Path, session_id: &str, transcript: &Path) {
        // 老状态文件里没有 `claude_pid`（`#[serde(default)]` ⇒ `None`）。多数用例关心的是
        // 别的通路，保持 `None` 就等于"不做僵尸判定"；要测僵尸判定用下面那个。
        state_file_with_pid(dir, session_id, transcript, None);
    }

    fn state_file_with_pid(
        dir: &Path,
        session_id: &str,
        transcript: &Path,
        claude_pid: Option<u32>,
    ) {
        state_file_full(dir, session_id, transcript, claude_pid, None);
    }

    /// 全字段的那个：`nested` 也要能指定（其余辅助函数都转调它）。
    fn state_file_full(
        dir: &Path,
        session_id: &str,
        transcript: &Path,
        claude_pid: Option<u32>,
        nested: Option<bool>,
    ) {
        let s = SessionState {
            session_id: session_id.into(),
            cwd: "D:\\w".into(),
            transcript_path: Some(transcript.to_string_lossy().to_string()),
            display_name: None,
            claude_pid,
            nested,
            state: "working".into(),
            state_since: 1000,
            last_event: "UserPromptSubmit".into(),
            last_event_at: 1000,
            last_assistant_message: None,
            notification_message: None,
            // 盘上恒为 0：挂件不写状态文件（Ruling #6），所以 offset 只能靠内存复用
            transcript_offset: 0,
        };
        crate::state::save_atomic(dir, &s).unwrap();
    }

    // ---- 选字体（本任务唯一"必须单测"的纯逻辑）--------------------------------

    #[test]
    fn pick_font_takes_the_first_candidate_that_works() {
        let d = tempdir("pickfirst");
        let missing = d.join("nope.ttf");
        let second = d.join("second.ttf");
        fake_font(&second, b"\x00\x01\x00\x00");
        let third = d.join("third.ttf");
        fake_font(&third, b"OTTO");

        let got = pick_font(&[missing, second.clone(), third]).unwrap();
        assert_eq!(got.0, second, "第一个可用的候选必须胜出");
        assert!(!got.1.is_empty());
    }

    #[test]
    fn pick_font_skips_files_that_are_not_fonts() {
        // egui 在字体解析失败时是 **panic**（不是回退），所以明显不是字体的数据
        // 必须在交给它之前挡掉：截断文件、写错路径的文本文件、空文件。
        let d = tempdir("pickjunk");
        let junk = d.join("a.ttf");
        fs::write(&junk, b"<html>not a font</html>").unwrap();
        let empty = d.join("b.ttf");
        fs::write(&empty, b"").unwrap();
        let good = d.join("c.ttf");
        fake_font(&good, b"\x00\x01\x00\x00");

        let got = pick_font(&[junk, empty, good.clone()]).unwrap();
        assert_eq!(got.0, good);
    }

    #[test]
    fn pick_font_returns_none_instead_of_panicking() {
        let d = tempdir("picknone");
        assert!(pick_font(&[]).is_none());
        assert!(pick_font(&[d.join("missing1.ttf"), d.join("missing2.ttc")]).is_none());
    }

    // ---- 头部自洽性校验（第二版修掉的洞）----------------------------------------
    //
    // 起因是一次 scoped 复审：`looks_like_font` 的注释声称挡住"截断文件"，实际只看
    // 4 字节 magic —— 而截断**恰恰保留** magic。后果不是理论上的：epaint 解析失败时
    // `panic!`（fonts.rs:990），GUI 子系统下没有控制台，用户看到的是"双击了，什么都
    // 没发生"。下面每条对应一个具体的失效形状。

    #[test]
    fn truncated_font_bytes_are_rejected() {
        // 反例的形状：magic 完好、长度残缺。旧实现全部放行，然后在第一帧 panic。
        for len in [4usize, 8, 12, 16, 24, 32] {
            let mut b = b"\x00\x01\x00\x00".to_vec();
            b.resize(len, 0);
            assert!(
                !looks_like_font(&b),
                "{len} 字节的截断 sfnt 必须被挡下：magic 对、结构残缺，放行会 panic"
            );
        }
        // 同形状的 TTC 残桩（复审在真机上复现过的那一种：截到 4/12/16/32 字节都 panic）。
        for len in [4usize, 8, 12, 16, 24, 32] {
            let mut b = b"ttcf".to_vec();
            b.resize(len, 0);
            assert!(!looks_like_font(&b), "{len} 字节的截断 ttcf 必须被挡下");
        }
    }

    #[test]
    fn a_coherent_header_is_accepted() {
        // 与上一条成对：校验不能严到把正常字体也挡掉。
        for magic in [b"\x00\x01\x00\x00", b"OTTO", b"true", b"ttcf"] {
            let d = tempdir("coherent");
            let p = d.join("f.ttf");
            fake_font(&p, magic);
            assert!(
                looks_like_font(&fs::read(&p).unwrap()),
                "{:?} 这种头部自洽的字体被误判了",
                std::str::from_utf8(magic).unwrap_or("<binary>")
            );
        }
    }

    #[test]
    fn sfnt_claiming_more_tables_than_it_holds_is_rejected() {
        let mut b = b"\x00\x01\x00\x00".to_vec();
        b.extend_from_slice(&100u16.to_be_bytes()); // numTables = 100 ⇒ 需要 1612 字节
        b.resize(64, 0);
        assert!(!looks_like_font(&b), "表目录声明超出文件长度，必须挡下");

        // 边界：numTables 恰好吃满长度就要放行（不能差一字节就把好字体误杀）。
        let mut exact = b"\x00\x01\x00\x00".to_vec();
        exact.extend_from_slice(&2u16.to_be_bytes());
        exact.resize(12 + 16 * 2, 0);
        assert!(looks_like_font(&exact), "刚好放得下的表目录不能被误判");
    }

    #[test]
    fn ttc_with_a_dangling_font_offset_is_rejected() {
        // numFonts = 0：一个字体都没有的集合，不是字体。
        let mut zero = b"ttcf".to_vec();
        zero.extend_from_slice(&[0, 1, 0, 0]);
        zero.extend_from_slice(&0u32.to_be_bytes());
        zero.resize(32, 0);
        assert!(!looks_like_font(&zero), "numFonts = 0 的 ttcf 必须挡下");

        // 偏移指向文件末尾之外。
        let mut dangling = b"ttcf".to_vec();
        dangling.extend_from_slice(&[0, 1, 0, 0]);
        dangling.extend_from_slice(&1u32.to_be_bytes());
        dangling.extend_from_slice(&9_999u32.to_be_bytes());
        dangling.resize(32, 0);
        assert!(!looks_like_font(&dangling), "字体偏移越界的 ttcf 必须挡下");

        // 偏移表本身就超出文件长度。
        let mut short_dir = b"ttcf".to_vec();
        short_dir.extend_from_slice(&[0, 1, 0, 0]);
        short_dir.extend_from_slice(&1_000_000u32.to_be_bytes());
        short_dir.resize(16, 0);
        assert!(
            !looks_like_font(&short_dir),
            "numFonts 大到装不下偏移表的 ttcf 必须挡下"
        );
    }

    #[test]
    fn every_real_candidate_font_on_this_machine_passes_the_check() {
        // **本组最重要的一条，也是上面几条的反向对照。**
        // 上面验的都是"该挡的挡住了"，这条验"不该挡的没挡"。后者更不能出错：
        // 校验过严 ⇒ 中文字体被拒 ⇒ 整屏缺字方框，而且不 panic、不报错，比崩溃更难
        // 发现（正是 spec §4.3 点名的失效模式）。
        //
        // 不能只靠 `the_ttc_actually_loads_and_its_glyphs_are_the_ones_used` 兜底：
        // 那条在取不到字体时会**跳过并通过**，校验过严时它给的是假绿。
        let mut checked = 0usize;
        for p in font_candidates() {
            let Ok(bytes) = fs::read(&p) else { continue };
            assert!(
                looks_like_font(&bytes),
                "真实存在的候选字体 {p:?} 被校验挡掉了 —— 中文会退回方框"
            );
            checked += 1;
        }
        assert!(checked > 0, "本机一个候选字体都不存在，这条测试没有验到东西");
        eprintln!("已核验 {checked} 个真实候选字体全部通过头部校验");
    }

    #[test]
    fn the_no_cjk_message_lists_every_candidate_and_the_impact() {
        // "取不到中文字体"原先**完全沉默** —— 整屏缺字方框，不 panic 也不报错。
        // 现在有对话框了，而弹框本身要人点、测不了；所以至少把**正文**钉住：
        // 文案错了等于没说（账本第 77 条同一类）。
        let msg = no_cjk_message(&font_candidates());
        for p in font_candidates() {
            let shown = p.display().to_string();
            assert!(
                msg.contains(&shown),
                "候选 {shown} 没被列进对话框 —— 用户就不知道去哪儿补字体"
            );
        }
        // 必须说清两件事：坏的是哪一部分、哪一部分没坏。
        assert!(msg.contains("缺字方框"), "要讲明症状是什么：{msg}");
        assert!(
            msg.contains("内嵌"),
            "要讲明数字与计时不受影响（数码字体内嵌），否则用户会以为整个挂件都坏了：{msg}"
        );
    }

    #[test]
    fn the_digital_subset_and_cjk_are_registered_in_both_font_families_in_that_order() {
        // 只挂比例族的话，等宽文本里的中文仍是方框 —— 所以两个族都要挂。
        // 顺序是本轮改动的核心：**数码子集必须在最前**（它只含 `0-9` `:`，所以
        // 数字走它、其余字符逐字符落到后面），cjk 紧随其后（拉丁+中文都走它），
        // egui 自带留在末尾（emoji 等最后兜底）。
        let fonts = build_font_definitions(Some(vec![0u8; 16]));
        assert!(fonts.font_data.contains_key(DIGITS_NAME));
        assert!(fonts.font_data.contains_key(CJK_NAME));

        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let list = fonts.families.get(&fam).expect("两个族都要有字体列表");
            assert_eq!(
                list.first().map(String::as_str),
                Some(DIGITS_NAME),
                "{fam:?} 的第一项必须是数码子集：它不在最前，数字就不会走数码体"
            );
            assert_eq!(
                list.get(1).map(String::as_str),
                Some(CJK_NAME),
                "{fam:?} 的第二项必须是中文字体：数字以外的字符都靠它（含拉丁）"
            );
            assert!(
                list.iter().skip(2).any(|n| n == "Ubuntu-Light" || n == "Hack"),
                "{fam:?} 必须保留 egui 自带字体作最后兜底（emoji 之类）：{list:?}"
            );
        }
    }

    #[test]
    fn cjk_may_be_absent_but_the_digital_subset_is_always_installed() {
        // 非中文 Windows（候选表一个都取不到）时：中文会显示成缺字方框，但**数码子集
        // 不能跟着一起丢** —— 它内嵌在二进制里，永远在手上。
        for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let fonts = build_font_definitions(None);
            let list = &fonts.families[&fam];
            assert_eq!(list.first().map(String::as_str), Some(DIGITS_NAME));
            assert!(
                !list.iter().any(|n| n == CJK_NAME),
                "没有中文字体就不该有 cjk 这一项（否则 egui 会拿到空字节而 panic）"
            );
            assert!(!fonts.font_data.contains_key(CJK_NAME));
        }
    }

    /// 在 `pos` 处模拟"按下并拖一点"，跑完必要的几帧，返回**最后一帧**窗口系统收到的命令。
    ///
    /// 为什么要跑好几帧：egui 判断"开始拖拽"需要一个**移动阈值** —— 光按下不算
    /// （否则轻微抖动就会误判成拖拽）。所以序列是：空帧 → 移过去 → 按下 → 再移一点。
    ///
    /// ⚠️ 这里**走完整的面板**（`CentralPanel` + `panel_frame`），与 `App::ui` 的真实调用链一致。
    /// 既有的 `a_press_on_the_border_...` 直接调 `handle_window_drag`，**绕过了面板的内边距**，
    /// 所以它抓不到"最外一圈是死区"那个 bug（内容区 == 窗口，压根不存在那一圈）。
    fn commands_after_press_at(pos: egui::Pos2, w: f32, h: f32) -> Vec<egui::ViewportCommand> {
        commands_after_drag_at(pos, egui::vec2(14.0, 0.0), w, h)
    }

    /// 同上，但可以指定"拖多远"。
    fn commands_after_drag_at(
        pos: egui::Pos2,
        drag: egui::Vec2,
        w: f32,
        h: f32,
    ) -> Vec<egui::ViewportCommand> {
        let cjk = pick_font(&font_candidates()).map(|(_, bytes)| bytes);
        let ctx = ctx_with_fonts(build_font_definitions(cjk));
        // ⚠️ **`resizing` 必须活在所有帧之外**（与真实 `App` 的 `self.resizing` 一致）。
        // 第一版把它声明在每帧的闭包里 ⇒ 缩放会话跨不过一帧 ⇒ "按下那帧建会话、下一帧就没了"，
        // 于是永远只看到那次"原地不动"的命令（实测：断言拿到 (0,0)）。
        // **夹具的生命周期与真实对象不一致，测的就是另一个世界**（本轮第三次踩同类）。
        let mut resizing = None;
        // 光标由夹具摆布（真实 GetCursorPos 在无头环境里不可控）。
        // 无头环境窗口原点按 (0,0) 折算，所以"窗口内坐标 pos" 就是"屏幕坐标 pos"。
        let cur = FakeCursor::new();
        let src = cur.src();
        let mut frame = |events: Vec<egui::Event>| -> Vec<egui::ViewportCommand> {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h))),
                events,
                ..Default::default()
            };
            let mut full = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().frame(panel_frame()).show(ui, |ui| {
                    handle_window_drag(ui, &mut resizing, &src); // ← 与 `App::ui` 同一个调用点、同一层
                    draw_panel(ui, &[], &usage::LedgerView::default(), true, &config::Config::default());
                });
            });
            full.textures_delta.clear();
            full.viewport_output
                .get(&egui::ViewportId::ROOT)
                .map(|o| o.commands.clone())
                .unwrap_or_default()
        };
        cur.set(pos.x, pos.y);
        frame(vec![]);
        frame(vec![egui::Event::PointerMoved(pos)]);
        // ⚠️ 返回**按下帧与移动帧的并集**：按下那一帧 `drag_started` 就已经为真、
        // 会先发一次"还没移动"的几何（等于原地不动），真正带位移的命令在下一帧。
        // 只取"第一个非空帧"会拿到那个空转的，断言就永远看到 (0,0)（实测踩过）。
        let mut cmds = frame(vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        }]);
        // 光标**真的动了** —— 这是位移的唯一来源（几何不再从指针的窗口内坐标推）。
        cur.set(pos.x + drag.x, pos.y + drag.y);
        cmds.extend(frame(vec![egui::Event::PointerMoved(pos + drag)]));
        cmds
    }

    /// 一个**可以被测试推着走**的光标（屏幕坐标）。真实 `GetCursorPos` 在无头环境里
    /// 不可控，而本次事故的关键恰恰是"**光标不动时几何不许变**"—— 那必须能精确摆布光标。
    struct FakeCursor(std::rc::Rc<std::cell::Cell<egui::Vec2>>);

    impl FakeCursor {
        fn new() -> Self {
            Self(std::rc::Rc::new(std::cell::Cell::new(egui::Vec2::ZERO)))
        }
        fn set(&self, x: f32, y: f32) {
            self.0.set(egui::vec2(x, y));
        }
        fn src(&self) -> impl Fn() -> Option<egui::Vec2> + '_ {
            let c = self.0.clone();
            move || Some(c.get())
        }
    }

    /// **缩放必须收敛**：光标在屏幕上不动时，窗口几何一帧都不许变。
    ///
    /// 这是 2026-09-17「拖动很慢 → 然后卡死」的**回归测试**。真实数据（自动模拟按住左缘
    /// 不动 6 秒）：窗口 5942 → 6582pt 一路涨、原点跑到 x = −4020、每帧固定 +8pt。
    ///
    /// ⚠️ 夹具**必须照实模拟真实对象的两件事**，否则测的是另一个世界：
    /// ① 窗口系统会**真的按我们的命令移动窗口**（所以 `viewport().outer_rect` 每帧在变）；
    /// ② 拖动期间 egui 手里那份**指针窗口内坐标会僵在旧值**（实测 `ptr` 全程 = 1 而窗口在跑）
    ///    —— 这里就故意一直喂同一个旧值，看几何会不会被它带跑。
    ///
    /// 修复前这条会红：几何由"egui 报的窗口原点 + 僵住的指针"推出来 ⇒ 每帧多推一点 ⇒ 发散。
    #[test]
    fn resizing_from_the_left_edge_converges_while_the_cursor_stands_still() {
        let cjk = pick_font(&font_candidates()).map(|(_, bytes)| bytes);
        let ctx = ctx_with_fonts(build_font_definitions(cjk));
        let (w, h) = (320.0, 380.0);
        let origin0 = egui::pos2(1000.0, 300.0);
        // 光标**钉死在屏幕上**：全程一动不动。
        let cur = FakeCursor::new();
        cur.set(origin0.x + 3.0, origin0.y + h / 2.0);
        let src = cur.src();

        let mut resizing = None;
        let mut origin = origin0; // 窗口系统眼里的原点（被我们的命令推着走）
        let mut widths: Vec<f32> = Vec::new();

        let mut frame =
            |origin: egui::Pos2, events: Vec<egui::Event>| -> Vec<egui::ViewportCommand> {
                let mut raw = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::pos2(0.0, 0.0),
                        egui::vec2(w, h),
                    )),
                    events,
                    ..Default::default()
                };
                raw.viewports.insert(
                    egui::ViewportId::ROOT,
                    egui::ViewportInfo {
                        outer_rect: Some(egui::Rect::from_min_size(origin, egui::vec2(w, h))),
                        inner_rect: Some(egui::Rect::from_min_size(origin, egui::vec2(w, h))),
                        ..Default::default()
                    },
                );
                let mut full = ctx.run_ui(raw, |ui| {
                    egui::CentralPanel::default()
                        .frame(panel_frame())
                        .show(ui, |ui| {
                            handle_window_drag(ui, &mut resizing, &src);
                            draw_panel(ui, &[], &usage::LedgerView::default(), true, &config::Config::default());
                        });
                });
                full.textures_delta.clear();
                full.viewport_output
                    .get(&egui::ViewportId::ROOT)
                    .map(|o| o.commands.clone())
                    .unwrap_or_default()
            };

        // 按下：光标在窗口内 (7, h/2)，落在左缘的缩放带里（带宽 8pt）。
        // ⚠️ 之后每一帧都喂**同一个值** —— 这就是实测到的"指针窗口内坐标僵住"
        // （真机上 `ptr` 全程 = 1 而窗口在跑）。几何不许被它带跑。
        let stale = egui::pos2(7.0, h / 2.0);
        frame(origin, vec![egui::Event::PointerMoved(stale)]);
        frame(
            origin,
            vec![egui::Event::PointerButton {
                pos: stale,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            }],
        );

        for _ in 0..12 {
            let cmds = frame(origin, vec![egui::Event::PointerMoved(stale)]);
            if let Some(p) = cmds.iter().find_map(|c| match c {
                egui::ViewportCommand::OuterPosition(p) => Some(*p),
                _ => None,
            }) {
                origin = p; // 窗口系统把命令应用了
            }
            if let Some(s) = cmds.iter().find_map(|c| match c {
                egui::ViewportCommand::InnerSize(s) => Some(*s),
                _ => None,
            }) {
                widths.push(s.x);
            }
        }

        assert!(
            !widths.is_empty(),
            "这 12 帧里必须真的发出过窗口命令 —— 空的话测的是空气，不是收敛"
        );
        let first = widths[0];
        for (i, wd) in widths.iter().enumerate() {
            assert!(
                (wd - first).abs() < 0.5,
                "第 {} 帧宽度跑到 {wd}（起始 {first}）—— **光标没动，几何就不许动**",
                i + 1
            );
        }
    }

    /// **最小高度下必须放得下"顶栏 + 一条完整会话"**，且不画出窗口外。
    ///
    /// 用户 2026-09-17 要"设置最小的大小限制"。下限的判据是**能看清一条会话**，
    /// 而不是"别压成一条缝" —— 后者是旧值 120 的理由，而 120 连一条会话都装不下。
    #[test]
    fn the_minimum_height_fits_the_header_and_one_whole_session() {
        let mut a = row("会话甲", State::Working, 3599);
        a.subagent_count = 3;
        let b = row("会话乙", State::Done, 12);
        let shapes = run_frame_sized(
            &[a, b],
            MIN_INNER_WIDTH,
            MIN_INNER_HEIGHT,
            &config::Config::default(),
        );
        let rects = drawn_rects(&shapes);
        let bottom = |n: &str| {
            rects
                .iter()
                .find(|(t, _)| t == n)
                .map(|(_, r)| r.bottom())
                .unwrap_or_else(|| panic!("最小高度下没画出 {n:?}"))
        };
        // 顶栏与第一行的**第二行**（`上下文 N%`）都要在窗口内 —— 后者是"一条会话画完整了"的标志
        assert!(bottom("● claude 会话 · 2 个") < MIN_INNER_HEIGHT);
        let ctx_bottom = bottom("上下文 36%");
        assert!(
            ctx_bottom < MIN_INNER_HEIGHT,
            "最小高度 {MIN_INNER_HEIGHT} 下，第一条会话的第二行画到了 {ctx_bottom}（窗口外）—— 下限太小了"
        );
        // 第二条会话装不下 ⇒ **整个不画**（宁可看不到，也别画半条 —— 半条看起来像界面坏了）
        assert!(
            !rects.iter().any(|(t, _)| t == "会话乙"),
            "装不下的会话必须整个不画：{rects:?}"
        );
    }

    /// **看得见的几条会话必须两两不同色** —— 哪怕列表里还有更多（含已退出的会话）。
    ///
    /// ⚠️ 这条用例的 session_id 是**挑过的**（`t0-0 … t0-7`）：整表分配时它们前 4 条会撞成
    /// `[0,3,2,2]`（第 3、4 条同色），而"只按画得下的几条分配"给的是 `[0,3,2,1]`。
    /// **第一版随手用了 `sess-0…7`，两种实现都过 —— 那是空断言**（比没有测试更坏：
    /// 它让人以为这条保证有人守着）。
    #[test]
    fn the_visible_sessions_are_all_differently_coloured_even_if_more_sessions_exist() {
        let rows: Vec<view::Row> = (0..8)
            .map(|i| {
                let mut r = row(&format!("会话{i}"), State::Done, 10 * (i + 1));
                r.session_id = format!("t0-{i}");
                r.context_pct = Some(0.1 * (i + 1) as f32);
                r
            })
            .collect();
        let shapes = run_frame_sized(&rows, DEFAULT_INNER_W, DEFAULT_INNER_H, &config::Config::default());
        let drawn = bar_colors_by_name(&shapes);
        assert_eq!(drawn.len(), 4, "默认尺寸下该画出 4 条：{drawn:?}");
        let uniq: std::collections::HashSet<_> =
            drawn.iter().map(|(_, c)| (c.r(), c.g(), c.b())).collect();
        assert_eq!(
            uniq.len(),
            drawn.len(),
            "画出来的 {} 条里有同色的（实测 {drawn:?}）",
            drawn.len()
        );
    }

    /// ⭐ **用户什么都没做，颜色就不许变。**
    ///
    /// 立这条是因为老实现（只对着"这一屏画得下的那几条"分配）会在两种情况下把已经上屏的
    /// 颜色整体重排：**窗高变一变**（`fit` 变 ⇒ 可见集合的成员变）、**排序变一变**
    /// （某条会话改了状态 ⇒ 挤进去/被挤出来）。色块唯一的职责是回答"这是哪条会话"，
    /// 颜色一跳就答不出来了。
    ///
    /// 夹具是**挑过的**（`session_id` 取自真机状态目录），且用例里**内建反向对照**：
    /// 老算法在同一组输入上给出的两组颜色**必须不同** —— 否则这条用例对老实现也照样绿，
    /// 那就是空断言（本仓踩过：`sess-0…7` 两个实现都过）。
    #[test]
    fn a_session_keeps_its_colour_when_the_visible_count_changes() {
        // 这四个 id 是**搜出来的**（哈希色下标 2/3/1/3）：老算法在"可见 3 条"与"可见 4 条"
        // 两种情况下给第 2 条的颜色分别是 3 与 0 —— 夹具只有挑成这样，才分得出新旧实现。
        let ids = [
            "92276658-1e27-a1c0-8a6a-63ec24ede6a4",
            "ae97ba94-d0ed-a82f-8f6d-05584ef8aa38",
            "923a7369-94e3-bf91-1a61-dbe22e44158b",
            "18f135d2-5f55-7203-3018-50c5a38fd547",
        ];
        let rows: Vec<view::Row> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut r = row(&format!("会话{i}"), State::Done, 10 * (i as i64 + 1));
                r.session_id = (*id).to_string();
                r
            })
            .collect();

        // 老实现：只对着可见的那几条分配（`fit` 一变，输入集合就变）
        let old_style = |fit: usize| -> Vec<egui::Color32> {
            greedy_palette(&rows[..fit], &want_of(&rows[..fit]))
                .into_iter()
                .map(|i| SESSION_COLORS[i])
                .collect()
        };
        assert_ne!(
            &old_style(3)[..3],
            &old_style(4)[..3],
            "夹具必须能区分新旧实现：老算法在这两种可见条数下就该换过色"
        );

        // 新实现：全局定色 + 只在可见集合撞色时才局部重排
        let global = assign_colors(&rows);
        let three = colors_for_display(&rows, &global, 3);
        let four = colors_for_display(&rows, &global, 4);
        assert_eq!(
            &three[..3],
            &four[..3],
            "可见条数从 3 变到 4，前 3 条的颜色必须一个像素都不动"
        );
        // 可见的仍然两两不同（老保证不许丢）
        let uniq: std::collections::HashSet<_> =
            four.iter().map(|c| (c.r(), c.g(), c.b())).collect();
        assert_eq!(uniq.len(), four.len(), "可见的 4 条仍然必须互不同色");
    }

    /// ⭐ **全局分配必须与行序无关** —— 排序一变（某条会话改了状态），颜色不许跟着变。
    ///
    /// 定序用的是 `(期望色, session_id)`：`session_id` 稳定，行下标不稳定。把它换成行下标，
    /// 这条就会红 —— 那正是"列表一重排，整片颜色跟着变"的机制。
    #[test]
    fn the_global_assignment_ignores_the_row_order() {
        let ids = [
            "92276658-1e27-a1c0-8a6a-63ec24ede6a4",
            "ae97ba94-d0ed-a82f-8f6d-05584ef8aa38",
            "923a7369-94e3-bf91-1a61-dbe22e44158b",
            "18f135d2-5f55-7203-3018-50c5a38fd547",
        ];
        let mk = |id: &str| {
            let mut r = row("会话", State::Done, 100);
            r.session_id = id.to_string();
            r
        };
        let in_order: Vec<view::Row> = ids.iter().map(|i| mk(i)).collect();
        let reversed: Vec<view::Row> = ids.iter().rev().map(|i| mk(i)).collect();

        let by_id = |rows: &[view::Row]| -> Vec<(String, (u8, u8, u8))> {
            let colors = assign_colors(rows);
            let mut v: Vec<(String, (u8, u8, u8))> = rows
                .iter()
                .zip(colors)
                .map(|(r, c)| (r.session_id.clone(), (c.r(), c.g(), c.b())))
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            by_id(&in_order),
            by_id(&reversed),
            "同一条会话在两种行序下必须拿到同一个颜色"
        );
    }

    /// ⭐ 端到端（**钉界面上量到的数**）：同一批会话，窗口高一点/矮一点 ⇒ 画出来的条数不同，
    /// 但**两次都画出来的那几条，颜色必须一模一样**。
    ///
    /// 这是用户看得见的那一半（"我什么都没做，颜色自己变了"）。老实现（对着可见的几条分配）
    /// 在同样两种窗高下会给第 2 条不同的颜色 —— 夹具是挑过的，见上一条用例的说明。
    #[test]
    fn resizing_the_window_does_not_recolour_the_rows_that_stay_visible() {
        let ids = [
            "92276658-1e27-a1c0-8a6a-63ec24ede6a4",
            "ae97ba94-d0ed-a82f-8f6d-05584ef8aa38",
            "923a7369-94e3-bf91-1a61-dbe22e44158b",
            "18f135d2-5f55-7203-3018-50c5a38fd547",
        ];
        let rows: Vec<view::Row> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut r = row(&format!("会话{i}"), State::Done, 10 * (i as i64 + 1));
                r.session_id = (*id).to_string();
                r
            })
            .collect();

        // 窗高 380 = 默认，画 4 条；286 少画一条（`row_advance` 约 94pt）。两个数都断言，
        // 免得哪天行高常量一动，这条就悄悄退化成"渲染了同一个画面"。
        let tall = bar_colors_by_name(&run_frame_sized(&rows, DEFAULT_INNER_W, DEFAULT_INNER_H, &config::Config::default()));
        let short = bar_colors_by_name(&run_frame_sized(&rows, DEFAULT_INNER_W, 286.0, &config::Config::default()));
        assert_eq!(tall.len(), 4, "默认窗高该画 4 条：{tall:?}");
        assert_eq!(short.len(), 3, "286pt 该画 3 条：{short:?}");

        for (name, c) in short.iter() {
            let (_, c2) = tall
                .iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("{name} 在高窗口里不见了"));
            assert_eq!(
                (c.r(), c.g(), c.b()),
                (c2.r(), c2.g(), c2.b()),
                "{name} 在窗口变矮前后换了色"
            );
        }
    }

    /// **按在文字上也要能拖动窗口** —— 用户 2026-09-17 报的"不能拖动"就是这个。
    ///
    /// egui 的 `Label` 默认 `selectable_labels = true`，会给自己的 sense **加上
    /// `click_and_drag`**（为了鼠标选字）。标签画在拖动区**之上** ⇒ 按在任意文字上都被
    /// 标签抢走拖拽，窗口纹丝不动。面板上几乎每个像素都有文字，体感就是"根本拖不动"。
    ///
    /// 这条用例按在**顶栏文字**上（`● claude 会话 · …`），断言仍然发出 `StartDrag`。
    /// 少了它，把 `selectable_labels` 改回默认（或有人"顺手"恢复文字选中）不会有人发现。
    #[test]
    fn pressing_on_text_still_drags_the_window() {
        let cmds = commands_after_press_at(egui::pos2(40.0, 16.0), 320.0, 360.0);
        assert!(
            cmds.iter().any(|c| matches!(c, egui::ViewportCommand::StartDrag)),
            "按在文字上必须仍能拖动窗口（标签不许抢走拖拽）：{cmds:?}"
        );
    }

    /// **默认尺寸下四条会话要全部显示**（用户 2026-09-17："四个会话为什么只显示了 3 个"）。
    ///
    /// 起因是"装不下就不画"那条规矩**把 `ROW_BOTTOM_GAP` 也算进了行高** —— 判第 4 条
    /// "装不下"，而它的文字离底边还有 4pt。行后的空白与分隔线属于"这一行**之后**的东西"，
    /// 不该参与"这条会话看不看得清"的判断。
    #[test]
    fn four_sessions_all_fit_at_the_default_size() {
        let rows: Vec<view::Row> = (0..4)
            .map(|i| {
                let mut r = row(&format!("会话{i}"), State::Done, 10 * (i + 1));
                r.context_pct = Some(0.1 * (i + 1) as f32);
                r
            })
            .collect();
        // 默认窗口 320×360
        let shapes = run_frame_sized(&rows, DEFAULT_INNER_W, DEFAULT_INNER_H, &config::Config::default());
        let texts = drawn_texts(&shapes);
        for i in 0..4 {
            assert!(
                texts.iter().any(|(t, _, _)| *t == format!("会话{i}")),
                "第 {} 条会话没画出来（默认尺寸下四条都该在）：{:?}",
                i + 1,
                texts.iter().map(|(t, _, _)| t.as_str()).collect::<Vec<_>>()
            );
        }
    }

    /// **贴着窗口边按下再拖，窗口真的被缩放** —— 端到端，断言的是**发给窗口系统的几何**。
    ///
    /// 背景（2026-09-17，用户连着两次报"不能缩放"）：
    /// · 第一层：可交互区与缩放判定都挂在**内容区**（比窗口小 8pt）⇒ 最外一圈按下去毫无反应；
    /// · 第二层：判定对了之后发的 `BeginResize` 也**不生效** —— 它落到 winit 的
    ///   `WM_NCLBUTTONDOWN(HT*)`，靠窗口的"非客户区"判可缩放性，而无边框窗口正是把非客户区去掉换来的。
    ///   命令发出去、什么都没发生、也不报错（第三轮写下这条路时注明"只能人工验收"，**一直没人验收**）。
    ///
    /// 现在**自己算**：拖动期间每帧发 `OuterPosition` + `InnerSize`。
    /// 这条用例钉的就是那两个数：往左拖 14pt ⇒ **原点左移 14、宽度加 14**（右缘不动）。
    #[test]
    fn dragging_the_very_edge_resizes_the_window_and_keeps_the_opposite_edge_fixed() {
        for (name, pos, dx, dy, want_min, want_size) in [
            // 左缘：原点 x -14、宽 +14（右缘钉住）
            ("左缘往左拖 14", egui::pos2(1.0, 180.0), -14.0, 0.0, (-14.0, 0.0), (334.0, 360.0)),
            // 右缘：原点不动、宽 +14
            ("右缘往右拖 14", egui::pos2(319.0, 180.0), 14.0, 0.0, (0.0, 0.0), (334.0, 360.0)),
            // 上缘：原点 y -14、高 +14（下缘钉住）
            ("上缘往上拖 14", egui::pos2(160.0, 1.0), 0.0, -14.0, (0.0, -14.0), (320.0, 374.0)),
            // 右下角：两轴一起
            ("右下角往右下拖 14", egui::pos2(319.0, 359.0), 14.0, 14.0, (0.0, 0.0), (334.0, 374.0)),
        ] {
            let cmds = commands_after_drag_at(pos, egui::vec2(dx, dy), 320.0, 360.0);
            // 取**最后一次**（按下帧那次是"原地不动"，没有位移）
            let outer = cmds.iter().rev().find_map(|c| match c {
                egui::ViewportCommand::OuterPosition(p) => Some(*p),
                _ => None,
            });
            let size = cmds.iter().rev().find_map(|c| match c {
                egui::ViewportCommand::InnerSize(s) => Some(*s),
                _ => None,
            });
            let (Some(outer), Some(size)) = (outer, size) else {
                panic!("{name}：应当发出 OuterPosition + InnerSize，实测命令 {cmds:?}");
            };
            assert!(
                (outer.x - want_min.0).abs() < 0.5 && (outer.y - want_min.1).abs() < 0.5,
                "{name}：原点应为 {want_min:?}，实测 ({}, {})",
                outer.x,
                outer.y
            );
            assert!(
                (size.x - want_size.0).abs() < 0.5 && (size.y - want_size.1).abs() < 0.5,
                "{name}：尺寸应为 {want_size:?}，实测 ({}, {})",
                size.x,
                size.y
            );
        }
    }

    /// **缩放到最小尺寸就停住**，而且**被拖的那条边顶回去**（不能一边缩一边飘）。
    ///
    /// 用户 2026-09-17："设置最小的大小限制"。
    #[test]
    fn resizing_stops_at_the_minimum_size_and_keeps_the_anchored_edge() {
        // 左缘往右狂拖 400pt：右缘钉在 x=320，宽度夹到 MIN_INNER_WIDTH
        let cmds = commands_after_drag_at(egui::pos2(1.0, 180.0), egui::vec2(400.0, 0.0), 320.0, 360.0);
        let size = cmds
            .iter()
            .rev()
            .find_map(|c| match c {
                egui::ViewportCommand::InnerSize(s) => Some(*s),
                _ => None,
            })
            .expect("应当发出 InnerSize");
        let outer = cmds
            .iter()
            .rev()
            .find_map(|c| match c {
                egui::ViewportCommand::OuterPosition(p) => Some(*p),
                _ => None,
            })
            .expect("应当发出 OuterPosition");
        assert!(
            (size.x - MIN_INNER_WIDTH).abs() < 0.5,
            "宽度应夹在最小宽度 {MIN_INNER_WIDTH}，实测 {}",
            size.x
        );
        assert!(
            (outer.x - (320.0 - MIN_INNER_WIDTH)).abs() < 0.5,
            "右缘必须钉在原处（x=320）⇒ 原点应到 {}，实测 {}",
            320.0 - MIN_INNER_WIDTH,
            outer.x
        );
    }

    /// **贴边悬停时必须换成缩放光标** —— 用户 2026-09-17："鼠标放到边缘，光标没有变成可拖动的光标"。
    ///
    /// 无边框窗口**没有系统给的那份光标**，不自己设就永远是箭头。这条用例直接读
    /// `PlatformOutput::cursor_icon`（egui 交给窗口系统的那个值），所以中间任何一环断了都会红。
    #[test]
    fn hovering_the_edge_shows_a_resize_cursor() {
        for (name, pos, want) in [
            ("左缘", egui::pos2(1.0, 180.0), egui::CursorIcon::ResizeHorizontal),
            ("右缘", egui::pos2(319.0, 180.0), egui::CursorIcon::ResizeHorizontal),
            ("上缘", egui::pos2(160.0, 1.0), egui::CursorIcon::ResizeVertical),
            ("左上角", egui::pos2(1.0, 1.0), egui::CursorIcon::ResizeNwSe),
            ("右下角", egui::pos2(319.0, 359.0), egui::CursorIcon::ResizeNwSe),
            // 反向对照：面板**中间**不该是缩放光标（否则整块窗口都在喊"我是边"）
            ("中间", egui::pos2(160.0, 180.0), egui::CursorIcon::Default),
        ] {
            let got = cursor_after_hover_at(pos, 320.0, 360.0);
            assert_eq!(got, want, "{name}：光标不对（实测 {got:?}）");
        }
    }

    /// 在 `pos` 悬停一帧，返回这一帧交给窗口系统的光标形状。
    fn cursor_after_hover_at(pos: egui::Pos2, w: f32, h: f32) -> egui::CursorIcon {
        let cjk = pick_font(&font_candidates()).map(|(_, bytes)| bytes);
        let ctx = ctx_with_fonts(build_font_definitions(cjk));
        let frame = |events: Vec<egui::Event>| -> egui::CursorIcon {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h))),
                events,
                ..Default::default()
            };
            let mut resizing = None;
            let mut full = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().frame(panel_frame()).show(ui, |ui| {
                    handle_window_drag(ui, &mut resizing, &|| Some(egui::Vec2::ZERO));
                    draw_panel(ui, &[], &usage::LedgerView::default(), true, &config::Config::default());
                });
            });
            full.textures_delta.clear();
            full.platform_output.cursor_icon
        };
        frame(vec![]);
        frame(vec![egui::Event::PointerMoved(pos)])
    }

    /// **反向对照**：从面板**中间**按下要拖动窗口，不能变成缩放。
    ///
    /// 少了这条，把判定写成"一律 BeginResize"也能过 —— 那样窗口就再也拖不动了。
    #[test]
    fn pressing_in_the_middle_drags_the_window_instead_of_resizing_it() {
        let cmds = commands_after_press_at(egui::pos2(160.0, 180.0), 320.0, 360.0);
        assert!(
            cmds.iter().any(|c| matches!(c, egui::ViewportCommand::StartDrag)),
            "中间按下应当 StartDrag：{cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, egui::ViewportCommand::BeginResize(_))),
            "中间按下**不该**缩放（两者必须互斥，否则点边缘会同时拖动和缩放）：{cmds:?}"
        );
    }

    /// `window_rect` = 内容区外扩一个内边距 —— **必须是整块窗口**，
    /// 否则最外那一圈就成了死区（见 `handle_window_drag` 的注释）。
    #[test]
    fn the_interactive_rect_is_the_whole_window_not_the_content_area() {
        let cjk = pick_font(&font_candidates()).map(|(_, bytes)| bytes);
        let ctx = ctx_with_fonts(build_font_definitions(cjk));
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(320.0, 360.0))),
            ..Default::default()
        };
        let mut full = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default().frame(panel_frame()).show(ui, |ui| {
                let wr = window_rect(ui);
                assert_eq!(
                    (wr.left(), wr.top(), wr.right(), wr.bottom()),
                    (0.0, 0.0, 320.0, 360.0),
                    "窗口矩形必须正好是整块窗口（内容区是 {:#?}）",
                    ui.max_rect()
                );
            });
        });
        full.textures_delta.clear();
    }

    // ---- 外观（"纯白圆角面板"里可自动核对的那一半）------------------------------

    /// 面板外形：**纯白、不透明、无描边、有圆角**。
    ///
    /// ⚠️ 用例名与断言都改过：原来是 `..._with_a_thick_black_border`（白底**黑边**），
    /// **2026-09-16 用户推翻**："现在有一点点黑色的边框，我希望去掉，纯白色"。
    /// 观感只能人工验收；但"填纯白、alpha 255、描边宽度 0、圆角非零"都是事实。
    #[test]
    fn panel_frame_is_a_plain_white_rounded_rectangle() {
        let f = panel_frame();
        assert_eq!(f.fill, egui::Color32::WHITE, "面板必须填纯白");
        assert_eq!(
            f.stroke.width, 0.0,
            "面板**不许**有描边（2026-09-16：去掉黑边、纯白）"
        );
        // ⚠️ 这条与"圆角"是**两个独立的事**，2026-09-16 加圆角时特意再钉一次：
        // 窗口透明（`with_transparent`）只服务于"圆角切掉的那四个角"，
        // **面板填充必须仍是 alpha=255** —— 谁要是把这两件事混起来，这条会红。
        assert_eq!(f.fill.a(), 255, "白底必须完全不透明（窗口透明 ≠ 面板半透明）");
        // 圆角：用户 2026-09-16："挂件改为圆角"。**非零**就是结论 ——
        // "看起来圆不圆"只能人工验收，但"圆角半径是不是 0（= 根本没做）"是事实。
        assert!(
            f.corner_radius.nw >= 1,
            "面板必须有圆角（实测 nw = {}）—— 0 表示圆角没生效",
            f.corner_radius.nw
        );
        // ⚠️ 这条**在 2026-09-16 被用户推翻了**：原来是 `== CornerRadius::ZERO`（"框是直角的"），
        // 用户看过实机之后要"挂件改为圆角"。留着原来那句会立刻红 —— 已按新裁定改。
        assert!(
            f.corner_radius.nw > 0,
            "面板必须有圆角（0 = 圆角没生效）—— 用户 2026-09-16 裁定"
        );
        // 留白必须盖得住描边宽度，否则文字会贴到黑框上
        assert!(
            f.inner_margin.left as f32 >= f.stroke.width,
            "内边距（{}）不得小于描边宽度（{}）",
            f.inner_margin.left,
            f.stroke.width
        );
    }

    #[test]
    fn state_colors_stay_legible_on_a_white_panel() {
        // 旧配色是为深灰底挑的：琥珀 #e3b341 在白底上对比度只有 1.7:1，几乎看不见
        // ——正是用户这次抱怨的那类问题。白底必须重挑明度，且各状态要**互不相同**
        // （否则"一眼看出哪个在等待确认"这个功能就没了）。
        let all = [
            State::Working,
            State::Waiting,
            State::Error,
            State::Compacting,
            State::Done,
            State::Interrupted,
            State::Idle,
        ];
        for s in all {
            let c = state_color(s);
            let l = relative_luminance(c);
            let ratio = 1.05 / (l + 0.05);
            assert!(
                ratio >= 4.5,
                "{s:?} 的 {c:?} 在白底上只有 {ratio:.2}:1，看不清（要求 >= 4.5:1）"
            );
        }
        assert_ne!(state_color(State::Working), state_color(State::Waiting));
        assert_ne!(state_color(State::Error), state_color(State::Working));
        // 顺序语义：被打断（已过去）比待命（还活着）淡
        assert!(
            relative_luminance(state_color(State::Interrupted))
                > relative_luminance(state_color(State::Idle)),
            "被打断必须比待命更淡"
        );
    }

    /// WCAG 相对亮度（sRGB）。只为上面的对比度断言服务。
    fn relative_luminance(c: egui::Color32) -> f32 {
        let ch = |v: u8| {
            let v = v as f32 / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
    }

    #[test]
    fn font_sizes_are_the_ones_the_user_asked_for() {
        // 用户裁定：会名 17 / 状态 15（原来是 13 / 12）。字号是常量，直接钉住，
        // 免得以后"顺手"改回去。顶栏不在裁定里，保持 13。
        assert_eq!(NAME_SIZE, 17.0);
        assert_eq!(STATE_SIZE, 15.0);
        assert!(NAME_SIZE > STATE_SIZE, "会名要比状态大一号，层级才成立");
        // 第二行（`上下文 N%` + token + `子代理 N`）：2026-09-15 减半、2026-09-22 放大 1.5 倍。
        // 钉**字面量**而不是只钉那个比例式 —— 只写 `CONTEXT_SIZE == STATE_SIZE * 0.75` 的话，
        // 状态词一改，两边一起动，照样绿（本仓"空断言"的老毛病）。
        assert_eq!(CONTEXT_SIZE, 11.25);
        assert_eq!(TOKENS_SIZE, CONTEXT_SIZE, "token 那一格与 `上下文 N%` 同号字");
    }

    // ---- 真的画一遍：面板 + 一行的产出（不只是"配置对了"）-----------------------

    fn row(name: &str, state: State, elapsed: i64) -> view::Row {
        view::Row {
            session_id: "s1".into(),
            name: name.into(),
            state,
            elapsed_secs: elapsed,
            // 用户点名"不要显示"的**三样**（当前工具 / 步数 / 摘要）全都塞上非空值：
            // 它们**一个都不许出现在画出来的文字里**（否则又漏回界面了）。
            // `context_pct` 从第二轮起**要**显示，所以它不再是"不该出现"的一员。
            detail: Some("Bash · cargo test".into()),
            context_tokens: Some(363_026),
            context_limit: 1_000_000,
            context_pct: Some(0.36),
            progress: Progress::Steps(12),
            subtitle: Some("已修复排版，共 12 处".into()),
            subagent_count: 0,
            host_gone: false,
        }
    }

    /// 把一次 pass 产出的图形摊平（Shape 可嵌套在 `Shape::Vec` 里）。
    fn flatten(shapes: &[egui::epaint::ClippedShape], out: &mut Vec<egui::Shape>) {
        fn walk(s: &egui::Shape, out: &mut Vec<egui::Shape>) {
            match s {
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                other => out.push(other.clone()),
            }
        }
        shapes.iter().for_each(|cs| walk(&cs.shape, out));
    }

    /// 真正跑一帧：把**发货用的**面板与画行代码喂给一个无头 Context，然后检查
    /// 产出的图形。这样测到的是"确实画出来了"，不只是"Frame 配置对了"。
    ///
    /// ⚠️ 边界照实说：这里能断的是**填了白、描了黑、够粗**，以及**画了哪些字**。
    /// "白底黑边好不好看、数码体观感如何、字号够不够大"**无法单测**，由人工验收覆盖。
    fn run_frame(rows: &[view::Row]) -> Vec<egui::Shape> {
        run_frame_at(rows, 360.0)
    }

    /// 同上，但指定窗口宽度 —— 用来测"窄窗口下会名被截断、计时仍在框内"。
    ///
    /// 字体栈用**发货用的那条**（有中文字体就用它，没有就 [数码子集 + egui 自带]）：
    /// 会名的宽度必须按真实字体栈量出来才有意义（中文 17pt/字 vs 数码数字 12.24pt/字
    /// vs 雅黑拉丁 又是另一个值，差得很远）。
    fn run_frame_at(rows: &[view::Row], width: f32) -> Vec<egui::Shape> {
        run_frame_at_cfg(rows, width, &config::Config::default())
    }

    /// 同上，再指定配置（上下文占比的 warn/danger 阈值由它给）。
    fn run_frame_at_cfg(
        rows: &[view::Row],
        width: f32,
        cfg: &config::Config,
    ) -> Vec<egui::Shape> {
        run_frame_sized(rows, width, 260.0, cfg)
    }

    /// 真正跑一帧的**唯一实现**：宽、高、配置都给。
    ///
    /// ⚠️ **高度默认 260，那是个会骗人的值**（2026-09-16 复审实测）：一帧画不下的内容
    /// 会被 egui **静默裁掉**，而且**裁得不均匀** —— `ui.label` 里有一道
    /// `is_rect_visible` 的门（egui `widgets/label.rs`），画到可见区外就整段不画；
    /// 而 [`paint_right`] 走的是 `painter().galley(...)`，**没有这道门**，照样画。
    /// 实测 6 个会话：全量屏幕下 31 段文字全在，260pt 下只剩 21 段，且第 3~5 行
    /// **只剩右组的状态词与计时**（左组的会名与第二行全没了）。
    ///
    /// 后果是"会话多到装不下"这件事**在默认夹具里不是没测，而是测不了**：任何
    /// `expect("会名没画出来")` 都会 panic 在一个与被测量无关的原因上，而
    /// "右组还在"又会让人误判"靠下的部分也正常"。**要测多会话就自己传够高的 height。**
    fn run_frame_sized(
        rows: &[view::Row],
        width: f32,
        height: f32,
        cfg: &config::Config,
    ) -> Vec<egui::Shape> {
        run_frame_sized_with_ledger(rows, width, height, &usage::LedgerView::default(), cfg)
    }

    /// 同上，但**指定今日账本**（测"顶栏那个总数"和"行尾那个数"的用例用它）。
    fn run_frame_sized_with_ledger(
        rows: &[view::Row],
        width: f32,
        height: f32,
        ledger: &usage::LedgerView,
        cfg: &config::Config,
    ) -> Vec<egui::Shape> {
        run_frame_full(rows, width, height, ledger, true, None, cfg)
    }

    /// **首轮扫描还没落地**的那一帧（遗留 #6 的用例用它）。
    fn run_frame_before_the_first_scan(rows: &[view::Row]) -> Vec<egui::Shape> {
        run_frame_full(
            rows,
            360.0,
            260.0,
            &usage::LedgerView::default(),
            false,
            None,
            &config::Config::default(),
        )
    }

    /// **把宿主 egui 主题拨到指定那一个**再跑一帧（只有分隔线那条用例需要）。
    fn run_frame_with_theme(rows: &[view::Row], theme: egui::Theme) -> Vec<egui::Shape> {
        run_frame_full(
            rows,
            360.0,
            260.0,
            &usage::LedgerView::default(),
            true,
            Some(theme),
            &config::Config::default(),
        )
    }

    /// 真正跑一帧的**唯一实现**（其余 `run_frame_*` 都是它的薄壳）：
    /// 宽、高、账本、首扫标志、主题、配置全给。
    ///
    /// ⚠️ 再多一个旋钮就往长里长，所以**只加"会影响这一帧画出来的像素"的东西**；
    /// 谁要再加开关，先想想能不能用现成的那个表达。（本仓不给警告挂 `#[allow]`，
    /// 所以这里靠"参数别超过 clippy 默认的 7 个"来约束 —— 现在正好 7 个。）
    fn run_frame_full(
        rows: &[view::Row],
        width: f32,
        height: f32,
        ledger: &usage::LedgerView,
        first_scan_done: bool,
        theme: Option<egui::Theme>,
        cfg: &config::Config,
    ) -> Vec<egui::Shape> {
        let cjk = pick_font(&font_candidates()).map(|(_, bytes)| bytes);
        let ctx = ctx_with_fonts(build_font_definitions(cjk));
        // 显式偏好（不是 `ThemePreference::System`）⇒ **不受跑测试这台机器的系统主题影响**，
        // 谁在什么机器上跑都画同一张图。
        if let Some(theme) = theme {
            ctx.set_theme(theme);
        }
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(width, height),
            )),
            ..Default::default()
        };
        let mut full = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default()
                .frame(panel_frame())
                .show(ui, |ui| {
                    // 与 `App::ui` 调用的是**同一个** `draw_panel`。
                    // 夹具默认传一本**空账本**（`total = 0`、`day = None`）——
                    // 于是顶栏不写"全部 N"，行尾都是 `0`。要测 token 显示的用例
                    // 走 `run_frame_with_ledger`。
                    draw_panel(ui, rows, ledger, first_scan_done, cfg);
                });
        });
        full.textures_delta.clear(); // 不 clear 会 panic（见拖拽用例的注释）
        let mut out = Vec::new();
        flatten(&full.shapes, &mut out);
        out
    }

    fn painted_texts(shapes: &[egui::Shape]) -> String {
        shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::Text(t) => Some(t.galley.text()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// 一帧里画出来的每一段文字：内容、落点、宽度。用来判"它是不是真的还在框内"。
    fn drawn_texts(shapes: &[egui::Shape]) -> Vec<(String, egui::Pos2, f32)> {
        shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::Text(t) => Some((t.galley.text().to_owned(), t.pos, t.galley.size().x)),
                _ => None,
            })
            .collect()
    }

    /// 面板**内容区**的右边界（窗口宽 − 描边 − 内边距）—— 一行必须整体落在它左边。
    fn inner_right(width: f32) -> f32 {
        // 只扣内边距 —— 描边在 2026-09-16 被用户去掉了（见 `INNER_MARGIN`）
        width - INNER_MARGIN as f32
    }

    #[test]
    fn the_panel_really_paints_a_white_fill_inside_a_thick_black_border() {
        let shapes = run_frame(&[row("样例项目", State::Working, 72)]);
        let rects: Vec<&egui::epaint::RectShape> = shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::Rect(r) => Some(r),
                _ => None,
            })
            .collect();

        let panel = rects
            .iter()
            .find(|r| r.fill == PANEL_FILL && r.fill.a() == 255)
            .unwrap_or_else(|| {
                panic!(
                    "一帧里必须有一块纯白不透明的矩形：{:#?}",
                    rects.iter().map(|r| (r.fill, r.stroke)).collect::<Vec<_>>()
                )
            });
        assert_eq!(
            panel.stroke.width, 0.0,
            "面板**不许**有描边（用户 2026-09-16：「去掉黑色的边框，纯白色」）"
        );
    }

    #[test]
    fn a_row_draws_name_state_timer_and_always_the_context_line() {
        // 第一轮点名保留的三样 + 顶栏 + **本轮加回来的上下文占比**；
        // 以及**仍然不许出现的三样**（当前工具 / 步数 / 摘要）。
        // 会名特意用**短**的：本用例管"画了哪些字"，"长了怎么办"归下面的截断用例。
        let shapes = run_frame(&[row("样例项目", State::Waiting, 72)]);
        let text = painted_texts(&shapes);
        let has = |s: &str| text.contains(s);

        assert!(has("样例项目"), "会名必须画出来：{text}");
        assert!(has("等待确认"), "状态词必须画出来：{text}");
        assert!(has("01:12"), "计时必须画出来（72 秒 = 01:12）：{text}");
        assert!(has("claude 会话 · 1 个"), "顶栏必须保留、且英文小写：{text}");
        // 色块**不再是字形**（用户："色块稍微大一点" ⇒ 改画矩形，见 `BAR_WIDTH`）：
        // 它的存在由 `the_bar_colour_...` 那条用例按矩形断言 —— 这里不再查字符串。
        // ③ 上下文占比：**始终**显示（`row()` 给的是 0.36）
        assert!(has("上下文 36%"), "上下文占比必须始终画出来：{text}");

        for gone in [
            "Bash · cargo test", // detail：当前工具
            "第 12 步", // progress
            "任务 0/0",
            "已修复排版，共 12 处", // subtitle：摘要
            "子代理", // 没有子代理 → 这一整段不该出现（见下面的专门用例）
        ] {
            assert!(!has(gone), "{gone:?} 不该出现在界面上：{text}");
        }
    }

    #[test]
    fn the_header_and_every_visible_latin_word_are_lowercase() {
        // 用户第二轮要求"英文用小写"。全仓**界面可见**的拉丁词只有顶栏那一个
        // `claude`（其余文案全是中文），所以这条就是"上界"：把整帧画出来的文字
        // 里所有 ASCII 字母捞出来，断言它们全是小写。
        let shapes = run_frame(&[row("样例项目", State::Waiting, 72)]);
        let text = painted_texts(&shapes);
        let upper: Vec<char> = text.chars().filter(|c| c.is_ascii_uppercase()).collect();
        assert!(
            upper.is_empty(),
            "画出来的文字里出现了大写拉丁字母 {upper:?} —— 用户要求统一小写：{text}"
        );
        assert!(text.contains("● claude 会话"), "顶栏品牌名必须是小写 claude：{text}");
    }

    #[test]
    fn a_running_subagent_is_shown_on_the_context_line() {
        // ④ 有子代理时：数量要画出来，而且画在**与会名/状态/计时紧邻的那一行**上
        // （即上下文行），不新起第三行。
        let mut r = row("样例项目", State::Working, 72);
        r.subagent_count = 3;
        let shapes = run_frame(&[r]);
        let texts = drawn_texts(&shapes);
        let text = painted_texts(&shapes);
        assert!(text.contains("子代理 3"), "在跑的子代理数必须显示：{text}");
        assert!(text.contains("上下文 36%"), "上下文行仍在：{text}");

        let (ctx_pos, _) = texts
            .iter()
            .find(|(s, _, _)| s.starts_with("上下文"))
            .map(|(_, p, w)| (*p, *w))
            .expect("上下文必须被画出来");
        let (sub_pos, sub_w) = texts
            .iter()
            .find(|(s, _, _)| s.starts_with("子代理"))
            .map(|(_, p, w)| (*p, *w))
            .expect("子代理必须被画出来");
        assert!(
            (sub_pos.y - ctx_pos.y).abs() < 0.5,
            "子代理必须与上下文**同一行**（y 不同 = 多起了一行，行高会失控）：\
             上下文 y={} vs 子代理 y={}",
            ctx_pos.y,
            sub_pos.y
        );
        assert!(sub_pos.x >= ctx_pos.x, "子代理应排在上下文之后");

        // 行高守恒：有没有子代理，一行的总高度都不变（只是多了一段文字）。
        let without = run_frame(&[row("样例项目", State::Working, 72)]);
        let h = |shapes: &[egui::Shape]| {
            shapes
                .iter()
                .filter_map(|s| match s {
                    egui::Shape::Text(t) => Some(t.pos.y),
                    _ => None,
                })
                .fold(f32::MIN, f32::max)
                - shapes
                    .iter()
                    .filter_map(|s| match s {
                        egui::Shape::Text(t) => Some(t.pos.y),
                        _ => None,
                    })
                    .fold(f32::MAX, f32::min)
        };
        assert!(
            close(h(&shapes), h(&without)),
            "有/无子代理时文字块的总高度必须相同：{} vs {}",
            h(&shapes),
            h(&without)
        );
        assert!(sub_w > 0.0);
    }

    #[test]
    fn no_subagent_means_nothing_is_drawn_for_it() {
        // ④ 明确要求的**负向分支**：`subagent_count == 0` 时这一段**整个不画**
        // （不是画 `子代理 0`）。
        let shapes = run_frame(&[row("样例项目", State::Working, 72)]);
        let text = painted_texts(&shapes);
        assert!(
            !text.contains("子代理"),
            "没有子代理在跑时不该画出任何子代理文案：{text}"
        );
        // 而上下文那一段照常（两条规则互不牵连）
        assert!(text.contains("上下文 36%"), "上下文与子代理是独立的两段：{text}");
    }

    #[test]
    fn the_context_percentage_colours_follow_the_configured_thresholds() {
        // ③ 的配色：< warn 黑 / >= warn 琥珀 / >= danger 红，阈值取自 config
        // （warn_threshold = 50 / danger_threshold = 75）—— 这两个旋钮在本轮之前
        // 界面侧没有任何读者，这条用例正是它们的读者。
        let cfg = config::Config::default();
        assert_eq!((cfg.warn_threshold, cfg.danger_threshold), (50, 75));
        assert_eq!(context_color(Some(0.0), &cfg), TEXT_COLOR, "0% 用正文黑");
        assert_eq!(context_color(Some(0.49), &cfg), TEXT_COLOR, "< 50% 用正文黑");
        assert_eq!(context_color(Some(0.50), &cfg), state_color(State::Waiting), "50% 转琥珀");
        assert_eq!(context_color(Some(0.74), &cfg), state_color(State::Waiting));
        assert_eq!(context_color(Some(0.75), &cfg), state_color(State::Error), "75% 转红");
        assert_eq!(context_color(Some(0.99), &cfg), state_color(State::Error));
        assert_eq!(context_color(None, &cfg), TEXT_COLOR, "没有数字可着色");

        // 自己配一套阈值也必须生效（不是写死的 50/75）
        let strict = config::Config { warn_threshold: 10, danger_threshold: 20, ..cfg };
        assert_eq!(context_color(Some(0.15), &strict), state_color(State::Waiting));
        assert_eq!(context_color(Some(0.25), &strict), state_color(State::Error));

        // 颜色真的落到了画面上（不只是函数返回值）
        let mut hot = row("样例项目", State::Working, 72);
        hot.context_pct = Some(0.91);
        let shapes = run_frame(&[hot]);
        let color = shapes
            .iter()
            .find_map(|s| match s {
                egui::Shape::Text(t) if t.galley.text().starts_with("上下文") => {
                    t.galley.job.sections.first().map(|sec| sec.format.color)
                }
                _ => None,
            })
            .expect("上下文必须被画出来");
        assert_eq!(color, state_color(State::Error), "91% 那一行必须画成红色");
    }

    #[test]
    fn the_context_text_covers_the_unknown_case_and_rounds_to_whole_percent() {
        // 文字口径（纯函数，与字体无关）：
        assert_eq!(fmt_context(Some(0.363026)), "上下文 36%");
        assert_eq!(fmt_context(Some(0.68)), "上下文 68%");
        assert_eq!(fmt_context(Some(0.50)), "上下文 50%");
        assert_eq!(fmt_context(Some(0.999)), "上下文 100%");
        assert_eq!(fmt_context(Some(1.5)), "上下文 150%");
        // 读不到 usage 时**不编造 0%**：那是"未知"，不是"没用量"
        assert_eq!(fmt_context(None), "上下文 --");
    }

    #[test]
    fn the_context_line_appears_even_without_any_transcript_data() {
        // "全程显示"的极端情形：会话连 transcript 都还没读到（context_pct = None）。
        // 那一行也必须画出来，只是数字位置是 `--`。
        let mut r = row("样例项目", State::Working, 72);
        r.context_pct = None;
        let shapes = run_frame(&[r]);
        let text = painted_texts(&shapes);
        assert!(text.contains("上下文 --"), "读不到也要画这一行：{text}");
    }

    #[test]
    fn fmt_subagents_is_the_only_place_the_subagent_text_is_built() {
        assert_eq!(fmt_subagents(1), "子代理 1");
        assert_eq!(fmt_subagents(12), "子代理 12");
    }

    // ---- 会名截断（用户第二个顾虑："长会名把计时挤出窗口"）----------------------

    #[test]
    fn truncate_to_width_is_char_based_and_respects_the_budget() {
        // 用一把"每个字符 10 宽"的**假尺子**，把"截断逻辑"与"字体怎么量宽"解耦：
        // 本用例只钉截断本身（真字体栈的宽度由下面的窗口用例覆盖）。
        let measure = |s: &str| s.chars().count() as f32 * 10.0;

        // 放得下 → 原样返回，不画蛇添足加省略号
        assert_eq!(truncate_to_width("样例项目", 40.0, measure), "样例项目");
        assert_eq!(truncate_to_width("样例项目", 1000.0, measure), "样例项目");
        assert_eq!(truncate_to_width("", 50.0, measure), "");

        // 放不下 → 以 … 结尾、不超额度、是原名的前缀、且**极大**（再多留一个字就超）
        let name = "示例项目报告排版修复"; // 10 个字 = 100 宽
        for budget in [10.0, 20.0, 37.0, 60.0, 95.0] {
            let got = truncate_to_width(name, budget, measure);
            assert!(got.ends_with('…'), "budget={budget} 时必须以 … 结尾：{got:?}");
            assert!(
                measure(&got) <= budget,
                "budget={budget} 时 {got:?} 宽 {} 超了",
                measure(&got)
            );
            let kept: Vec<char> = got.chars().filter(|c| *c != '…').collect();
            assert!(
                name.chars().take(kept.len()).eq(kept.iter().copied()),
                "budget={budget} 时 {got:?} 不是原名的前缀"
            );
            let mut one_more: String = name.chars().take(kept.len() + 1).collect();
            one_more.push('…');
            assert!(
                measure(&one_more) > budget,
                "budget={budget} 时明明还放得下却只留了 {got:?}（应该能留到 {one_more:?}）"
            );
        }

        // **按字符而不是字节**：中文一个字 3 个 UTF-8 字节，按字节切会 panic 或切出半个字。
        // 这里逐个额度扫一遍，只要有一次返回非法 UTF-8（Rust 里不可能）或 panic 就挂；
        // 顺带钉住结果永远是完整字符。
        let multi = "中文会话名混排abc";
        for b in 0..=multi.len() * 10 {
            let got = truncate_to_width(multi, b as f32, measure);
            assert!(
                got.chars().all(|c| c == '…' || multi.contains(c)),
                "额度 {b} 时切出了半个字：{got:?}"
            );
        }

        // 额度小到连 … 都放不下 → 宁可空着（这一格全让给状态词与计时），也不要溢出
        assert_eq!(truncate_to_width("样例项目", 10.0, measure), "…");
        assert_eq!(truncate_to_width("样例项目", 9.9, measure), "");
        assert_eq!(truncate_to_width("样例项目", 0.0, measure), "");
        assert_eq!(truncate_to_width("样例项目", -5.0, measure), "");
    }

    #[test]
    fn a_long_name_is_ellipsized_and_the_timer_stays_inside_the_panel() {
        // 窄窗口 + 超长会名：判据是**计时与状态词完整可见**（用户本项的验收判据）。
        // 用拉丁名是为了在"有没有中文字体"的机器上都成立（拉丁走数码体，宽度真实）。
        const W: f32 = 200.0;
        let long = "web-lab-an-extremely-long-session-name";
        let shapes = run_frame_at(&[row(long, State::Waiting, 72)], W);
        let texts = drawn_texts(&shapes);

        let find = |want: &str| -> (egui::Pos2, f32) {
            texts
                .iter()
                .find(|(s, _, _)| s == want)
                .map(|(_, p, w)| (*p, *w))
                .unwrap_or_else(|| panic!("{want:?} 没有被画出来：{texts:?}"))
        };

        // ① 会名被截断，且以 … 收尾
        let (name, _, _) = texts
            .iter()
            .find(|(s, _, _)| s.ends_with('…'))
            .unwrap_or_else(|| panic!("长会名必须被截断成 … 结尾：{texts:?}"));
        assert!(long.starts_with(name.trim_end_matches('…')), "必须是原名前缀：{name:?}");

        // ② 状态词与计时**完整画出来**，而且右边界没有越出内容区
        for must in ["等待确认", "01:12"] {
            let (pos, w) = find(must);
            assert!(
                pos.x + w <= inner_right(W) + 0.5,
                "{must:?} 被挤出面板了：右边界 {} > 内容区右界 {}",
                pos.x + w,
                inner_right(W)
            );
        }
    }

    #[test]
    fn widening_the_window_lets_more_of_the_name_show() {
        // 用户要求"窗口被拉宽时，名字应能显示得更长"——额度是按当帧可用宽度现算的，
        // 这条钉住它真的跟着窗口走，没有写死宽度。
        let long = "web-lab-an-extremely-long-session-name";
        let name_at = |w: f32| -> String {
            let shapes = run_frame_at(&[row(long, State::Working, 72)], w);
            drawn_texts(&shapes)
                .iter()
                .map(|(s, _, _)| s.clone())
                .find(|s| long.starts_with(s.trim_end_matches('…')) && !s.is_empty())
                .unwrap_or_default()
        };
        let narrow = name_at(200.0);
        let wide = name_at(600.0);
        assert!(narrow.ends_with('…'), "窄窗口下应当被截断：{narrow:?}");
        assert!(
            wide.chars().count() > narrow.chars().count(),
            "窗口拉宽后名字必须显示得更长：窄 {narrow:?}（{} 字）vs 宽 {wide:?}（{} 字）",
            narrow.chars().count(),
            wide.chars().count()
        );
    }

    /// 在**真实字体栈**里量一段文字宽（需要一个 `Ui`，用一个无头 pass 现取）。
    fn measure_in_stack(ctx: &egui::Context, text: &str, size: f32) -> f32 {
        let mut w = f32::NAN;
        let mut full = ctx.run_ui(egui::RawInput::default(), |ui| {
            w = text_width(ui, text, size);
        });
        full.textures_delta.clear();
        w
    }

    #[test]
    fn a_chinese_name_is_measured_with_the_cjk_font_not_the_digital_one() {
        // 字体栈是 [数码子集, 微软雅黑]：中文与拉丁都走**雅黑**，数字与 `:` 走数码体，
        // 两个方向宽度差很多。拿错尺子（例如一律乘一个常数）会把"还能留几个字"算错。
        let Some((font, cjk_bytes)) = pick_font(&font_candidates()) else {
            eprintln!("跳过：本机没有可用的中文字体，中文宽度无从量起");
            return;
        };
        let ctx = ctx_with_fonts(build_font_definitions(Some(cjk_bytes)));

        // ① 同一个字号下，中文单字必须**明显宽于**拉丁单字（雅黑里 CJK = 1 em、
        //    拉丁 ~0.5 em）。若相等就说明有一边量错了。
        let (w_cjk, w_lat) = (
            measure_in_stack(&ctx, "骑", NAME_SIZE),
            measure_in_stack(&ctx, "a", NAME_SIZE),
        );
        assert!(
            w_cjk > w_lat * 1.15,
            "中文单字 {w_cjk} 与拉丁单字 {w_lat} 应当差得开（雅黑 17 对 ~8）；\
             若相等就说明有一边量错了（字体：{font:?}）"
        );

        // ② 同一个窗口、两个都**放不下**的名字：中文能被留下的字数**必须更少** ——
        //    这就是"中文确实按雅黑的宽度在量"的直接证据，且不需要知道额度是多少。
        //
        //    两个名字都足够长，保证都被截断（本轮把拉丁从数码体换成了雅黑，拉丁
        //    从 12.24pt/字符降到 ~9.4pt/字符 —— 上一版用的 15 字拉丁名在这里已经
        //    放得下了，会把本条断言变成"没截断"的假失败）。
        const W: f32 = 320.0;
        let kept_of = |name: &str| -> usize {
            let shapes = run_frame_at(&[row(name, State::Waiting, 72)], W);
            let text = drawn_texts(&shapes)
                .iter()
                .map(|(s, _, _)| s.clone())
                .find(|s| name.starts_with(s.trim_end_matches('…')) && s.ends_with('…'))
                .unwrap_or_else(|| panic!("{name:?} 没有被截断：{shapes:?}"));
            text.chars().filter(|c| *c != '…').count()
        };
        let kept_cjk = kept_of("示例项目报告排版修复与视觉");           // 13 个汉字
        let kept_lat = kept_of("web-lab-session-with-a-long-name"); // 32 个拉丁字符
        assert!(kept_cjk >= 1, "至少要留下一个字，实际 {kept_cjk}");
        assert!(
            kept_cjk < kept_lat,
            "中文留下 {kept_cjk} 字、拉丁留下 {kept_lat} 字 —— 拉丁没比中文多，\
             说明中文没用雅黑的宽度在量（字体：{font:?}）"
        );

        // ③ 本项的验收判据：状态词与计时完整落在内容区内。
        //    顺带把**本轮新加的第二行**也纳入：`上下文` 与（有子代理时的）`子代理 N`
        //    都是不能截断的数字，窄窗里同样不许越过内容区右界。
        let mut r = row("示例项目报告排版修复与视觉", State::Waiting, 72);
        r.context_pct = Some(0.91);
        r.subagent_count = 12;
        let texts = drawn_texts(&run_frame_at(&[r], W));
        for must in ["等待确认", "01:12", "上下文 91%", "子代理 12"] {
            let (pos, w) = texts
                .iter()
                .find(|(s, _, _)| s == must)
                .map(|(_, p, w)| (*p, *w))
                .unwrap_or_else(|| panic!("{must:?} 没有被画出来：{texts:?}"));
            assert!(
                pos.x + w <= inner_right(W) + 0.5,
                "{must:?} 被挤出面板了：右边界 {} > 内容区右界 {}",
                pos.x + w,
                inner_right(W)
            );
        }
    }

    #[test]
    fn an_empty_session_list_says_so_instead_of_drawing_nothing() {
        let shapes = run_frame(&[]);
        let text = painted_texts(&shapes);
        assert!(text.contains("没有活跃会话"), "空列表要有占位文案：{text}");
        assert!(text.contains("claude 会话 · 0 个"), "顶栏英文小写：{text}");
    }

    // ---- 轮询接线（真实 IO：状态文件 + transcript → 行）------------------------

    #[test]
    fn poll_once_wires_transcript_deltas_into_rows() {
        // 覆盖 T11 留下的三个空分支（detail / context_pct / progress）—— 这次走的是
        // 真实通路：状态目录里的状态文件 + 真实 transcript 文件。
        let d = tempdir("wire");
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
        let sessions = tempdir("wire-sessions");
        state_file(&sessions, "s1", &t);

        let cfg = config::Config::default();
        let rows = poll_once(
            &sessions,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &cfg,
            2000,
        );

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "示例项目报告排版修复", "aiTitle 必须成为会话名");
        assert_eq!(rows[0].detail.as_deref(), Some("Bash · cargo test"));
        assert_eq!(rows[0].context_tokens, Some(363_026));
        assert!((rows[0].context_pct.unwrap() - 0.363026).abs() < 1e-6);
        assert_eq!(rows[0].progress, Progress::Steps(1));
    }

    #[test]
    fn poll_once_reuses_offsets_so_later_polls_only_read_new_bytes() {
        // 状态文件每轮都从磁盘重读，而盘上的 `transcript_offset` 恒为 0（挂件按
        // Ruling #6 不写回）。若不复用内存里的 offset，每轮都全量重扫整份 transcript
        // —— 实测 5 个会话 88 MB 时每轮 719 ms（这是"卡顿"的真正主因）。
        //
        // 步数是这条的行为判据：
        //   复用内存 offset → 第二轮只读新增那一行：prior 1 + 1 = 2
        //   不复用（全量重扫）→ 第二轮重扫全文件：prior 1 + 2 = 3
        let d = tempdir("offsets");
        let t = d.join("t.jsonl");
        let line = |name: &str, arg: &str| {
            format!(
                r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"{name}","input":{{"command":"{arg}"}}}}]}}}}"#
            )
        };
        fs::write(&t, format!("{}\n", line("Read", "a.rs"))).unwrap();
        let sessions = tempdir("offsets-sessions");
        state_file(&sessions, "s1", &t);

        let cfg = config::Config::default();
        let mut deltas = HashMap::new();
        let mut offsets = HashMap::new();
        let mut dead_streak = HashMap::new();

        let first = poll_once(&sessions, &mut deltas, &mut offsets, &mut dead_streak, &cfg, 2000);
        assert_eq!(first[0].progress, Progress::Steps(1));
        assert!(offsets["s1"] > 0, "offset 必须推进（且只活在内存里）");

        let mut f = fs::OpenOptions::new().append(true).open(&t).unwrap();
        use std::io::Write;
        writeln!(f, "{}", line("Bash", "ls")).unwrap();
        drop(f);

        let second = poll_once(&sessions, &mut deltas, &mut offsets, &mut dead_streak, &cfg, 2000);
        assert_eq!(
            second[0].progress,
            Progress::Steps(2),
            "只读增量：不是重扫全文件得到的 3"
        );
    }

    // ---- 2026-09-18：子会话不占行（用户报"我明明是四个却显示六个"）----------------

    #[test]
    fn a_sub_session_spawned_by_another_session_is_not_listed() {
        // 真机形态（2026-09-18 取证）：用户开 4 个会话，其中一个在跑 token 实验，
        // 用 Bash 拉起两个 `claude -p` 子进程 —— 子进程**自己也是会话**、也写状态文件，
        // 于是界面列出 6 行。用户裁定：子会话不显示，只列用户自己开的。
        //
        // 这个事实由 hook 在 `SessionStart` 定好（`procinfo::host_and_parent`：
        // 宿主之上还有没有第二个 `claude.exe`），此处只读。
        let d = tempdir("nested");
        let sessions = tempdir("nested-sessions");
        let t = d.join("t.jsonl");
        fs::write(&t, "").unwrap();
        state_file_full(&sessions, "mine", &t, None, Some(false));
        state_file_full(&sessions, "child-mcp", &t, None, Some(true));
        state_file_full(&sessions, "child-plain", &t, None, Some(true));

        let cfg = config::Config::default();
        let mut deltas = HashMap::new();
        let mut offsets = HashMap::new();
        let mut dead_streak = HashMap::new();
        let rows = poll_once(&sessions, &mut deltas, &mut offsets, &mut dead_streak, &cfg, 2000);

        let ids: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, vec!["mine"], "子会话一行都不许占 —— 用户开几个就该显示几个");
    }

    #[test]
    fn a_session_without_the_nested_field_is_listed_not_hidden() {
        // 反向对照（这条是安全阀）：改造前写下的状态文件**没有** `nested` 这个字段，
        // 读出来是 `None` —— 必须**照旧列出来**。
        //
        // 若谁把判据写成"不是 `Some(false)` 就藏"（很容易顺手写成 `!= Some(false)`），
        // 用户盘上所有老会话文件会**整片消失**，而且是静默的（`list_all` 不报错）。
        let d = tempdir("oldstate");
        let sessions = tempdir("oldstate-sessions");
        let t = d.join("t.jsonl");
        fs::write(&t, "").unwrap();
        state_file_full(&sessions, "old", &t, None, None);

        let cfg = config::Config::default();
        let mut deltas = HashMap::new();
        let mut offsets = HashMap::new();
        let mut dead_streak = HashMap::new();
        let rows = poll_once(&sessions, &mut deltas, &mut offsets, &mut dead_streak, &cfg, 2000);

        assert_eq!(rows.len(), 1, "没有这个事实的老文件必须照常列出");
        assert_eq!(rows[0].session_id, "old");
    }

    // ---- 9b：僵尸会话（宿主 Claude Code 进程已消失）------------------------------

    /// 一个**不可能存在**的 pid：Windows 的 pid 是 4 的倍数，且远小于 `u32::MAX`。
    const NO_SUCH_PID: u32 = u32::MAX - 3;

    #[test]
    fn a_gone_host_is_marked_exited_for_one_poll_then_the_row_disappears() {
        // 用户 2026-09-15 裁定：**先标"已退出"，连续两轮确认后再移除** —— 一次误判只让
        // 标签变一下，不会让一个活着的会话从界面上消失。
        let d = tempdir("zombie");
        let sessions = tempdir("zombie-sessions");
        let t = d.join("t.jsonl");
        fs::write(&t, "").unwrap();
        state_file_with_pid(&sessions, "dead", &t, Some(NO_SUCH_PID));

        let cfg = config::Config::default();
        let mut deltas = HashMap::new();
        let mut offsets = HashMap::new();
        let mut dead_streak = HashMap::new();

        let first =
            poll_once(&sessions, &mut deltas, &mut offsets, &mut dead_streak, &cfg, 2000);
        assert_eq!(first.len(), 1, "第 1 轮**不能**移除，只标记（这是那条安全阀）");
        assert!(first[0].host_gone, "第 1 轮必须标成已退出");

        let second =
            poll_once(&sessions, &mut deltas, &mut offsets, &mut dead_streak, &cfg, 2000);
        assert!(second.is_empty(), "连续两轮确认之后才移除");
    }

    #[test]
    fn a_session_with_no_recorded_host_is_never_called_gone() {
        // `claude_pid` 为 `None` ⇒ **不做判定**，退回改造前的行为。
        // 这条是整套机制的安全阀：老状态文件、或 `SessionStart` 没抓到 pid 时，
        // 绝不能把会话判死 —— 宁可漏报僵尸，不可误杀活人。
        let d = tempdir("nohost");
        let sessions = tempdir("nohost-sessions");
        let t = d.join("t.jsonl");
        fs::write(&t, "").unwrap();
        state_file(&sessions, "s1", &t); // 这个辅助写下的就是 claude_pid = None

        let cfg = config::Config::default();
        let mut deltas = HashMap::new();
        let mut offsets = HashMap::new();
        let mut dead_streak = HashMap::new();
        for round in 0..5 {
            let rows =
                poll_once(&sessions, &mut deltas, &mut offsets, &mut dead_streak, &cfg, 2000);
            assert_eq!(rows.len(), 1, "第 {round} 轮：没有 claude_pid 的会话永不被移除");
            assert!(!rows[0].host_gone);
        }
    }

    #[test]
    fn a_host_that_comes_back_resets_the_streak() {
        // 反向对照：**判错一次不该累积成移除**。第 2 轮如果宿主又"活着"（进程回来了，
        // 或第 1 轮是误判），streak 必须归零、行必须留下。
        let d = tempdir("revive");
        let sessions = tempdir("revive-sessions");
        let t = d.join("t.jsonl");
        fs::write(&t, "").unwrap();
        state_file_with_pid(&sessions, "rev", &t, Some(NO_SUCH_PID));

        let cfg = config::Config::default();
        let mut deltas = HashMap::new();
        let mut offsets = HashMap::new();
        let mut dead_streak = HashMap::new();
        let _ = poll_once(&sessions, &mut deltas, &mut offsets, &mut dead_streak, &cfg, 2000);
        assert_eq!(dead_streak["rev"], 1);

        // 状态文件里把 pid 换成**本进程**的：它活着，只是名字不叫 claude.exe ——
        // 于是 `alive` 仍然判"不是宿主"，这一轮还是 gone。要造"回来了"，
        // 直接把 streak 归零的路径走一遍：换成 None（不判定）。
        state_file_with_pid(&sessions, "rev", &t, None);
        let rows = poll_once(&sessions, &mut deltas, &mut offsets, &mut dead_streak, &cfg, 2000);
        assert_eq!(dead_streak["rev"], 0, "宿主（或判定依据）回来了，streak 必须归零");
        assert_eq!(rows.len(), 1, "归零之后那一行必须留下");
        assert!(!rows[0].host_gone);
    }

    #[test]
    fn mark_host_gone_sinks_gone_rows_and_keeps_the_rest_in_order() {
        let mut rows = vec![
            row("a", State::Working, 10),
            row("b", State::Waiting, 20),
            row("c", State::Done, 30),
        ];
        // 用会名当 session_id，断言才读得懂
        for r in rows.iter_mut() {
            r.session_id = r.name.clone();
        }
        let gone: HashSet<String> = ["a".to_owned()].into_iter().collect();
        view::mark_host_gone(&mut rows, &gone);

        let order: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(order, vec!["b", "c", "a"], "僵尸行沉到最后，其余保持原有相对顺序");
        assert!(rows[2].host_gone, "被标的那一行才置位");
        assert!(!rows[0].host_gone && !rows[1].host_gone);
    }

    // ---- 2026-09-15 的四项外观裁定 ---------------------------------------------

    /// 每个文字块的高度。`drawn_texts` 只给宽度（用来判**字形来自谁**），
    /// 要判**字号**就得看高度 —— 字号变了宽度也变，但宽度还受字符串内容影响，不如高度直白。
    fn drawn_text_heights(shapes: &[egui::Shape]) -> Vec<(String, f32)> {
        shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::Text(t) => Some((t.galley.text().to_owned(), t.galley.size().y)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_context_line_is_three_quarters_of_the_state_word() {
        // 两笔用户裁定叠起来：2026-09-15"上下文和对应的占比**缩小 50%**"，
        // 2026-09-22"**放大 1.5 倍**" ⇒ 0.5 × 1.5 = **状态词的 0.75**。
        //
        // 判据取**渲染出来的字高之比**，不是断言那个常量 —— 常量对了而没被用上
        // （例如某处仍 `.size(STATE_SIZE)`）是这条测试要抓的。
        // 实测（2026-09-22）：ctx 12 / state 16 = 0.75 整。
        let rows = vec![row("AAA", State::Done, 10)];
        let h = drawn_text_heights(&run_frame_at(&rows, 320.0));
        let height_of = |needle: &str| {
            h.iter()
                .find(|(t, _)| t == needle)
                .map(|(_, y)| *y)
                .unwrap_or_else(|| panic!("没画出 {needle:?}"))
        };
        let (ctx, state) = (height_of("上下文 36%"), height_of("完成"));
        let ratio = ctx / state;
        assert!(
            (ratio - 0.75).abs() < 0.06,
            "占比行字高 {ctx} / 状态词字高 {state} = {ratio}，应约为 0.75（半个状态词再 ×1.5）"
        );
    }

    #[test]
    fn rows_are_spaced_twice_as_far_apart_as_before() {
        // 用户 2026-09-15 裁定："每个会话的间距**扩大一倍**"，结果 = **58pt**
        // （第二行 galley 原点 → 下一会话会名 galley 原点；同一处的**视觉**空隙是 50，
        // 差的那 8 是第二行自己的字高 —— 别拿这个数去和视觉量法比）。
        //
        // 钉的是**最终效果**而不是 `EXTRA_ROW_GAP` 那个常量：间隔是它与 rule()、
        // 行高、item_spacing 的合成，改其中任何一个都会让这条测试红 —— 而不是界面悄悄变回去。
        let rows = vec![row("AAA", State::Done, 10), row("BBB", State::Done, 20)];
        let texts = drawn_texts(&run_frame_at(&rows, 320.0));
        let y_of = |needle: &str| {
            texts
                .iter()
                .find(|(t, _, _)| t == needle)
                .map(|(_, p, _)| p.y)
                .unwrap_or_else(|| panic!("没画出 {needle:?}"))
        };
        let gap = y_of("BBB") - y_of("上下文 36%");
        assert!(
            (gap - 58.0).abs() < 0.5,
            "会话间距应为 58pt（改前 29 的两倍），实测 {gap}"
        );
    }

    #[test]
    fn the_header_no_longer_goes_through_the_digital_font() {
        // 用户 2026-09-15："大标题里的所有字体都统一，**不用数码**"。
        //
        // 判据是**宽度比对**（与会名那套一致）：`3` 在顶栏那条族里量出来的宽度，
        // 不能等于它在数码子集里的宽度。**并配一个对照组** —— 主族里的 `3` 必须
        // 仍然等于数码宽度，否则说明"数码子集整个没装上"，本用例就区分不出东西了。
        //
        // ⚠️ **两条做这条测试时必须知道的：**
        //
        // 1. 三个宽度**必须在同一字号下量**。第一版拿 13pt 的顶栏宽度比 15pt 的数码宽度，
        //    `close()` 必然不成立 → 断言恒真，变异体照样能过。
        // 2. **微软雅黑自带数字**，所以顶栏避开数码体靠的是"CJK 排在栈首、先把 `3` 接走"，
        //    不只是"栈里没有数码子集"。因此第一个变异体（把顶栏族换成主族、`3` 仍在
        //    CJK 之后）**行为完全没变**、测试照样绿 —— 那不是测试无效，是**变异无效**。
        //    真正能改变行为的变异是"把数码子集插到 CJK **之前**"，那个当场就红了。
        //    （两次都是实测出来的，不是推的。）
        let Some((_, cjk_bytes)) = local_cjk() else {
            eprintln!("跳过：本机一个中文字体都取不到");
            return;
        };
        let ship = ctx_with_fonts(build_font_definitions(Some(cjk_bytes)));
        let digits = ctx_digits_only();
        // ⚠️ **三个宽度必须在同一个字号下量。** 先前那版拿 `HEADER_SIZE`(13pt) 的顶栏宽度
        // 去比 `glyph_width`(写死 15pt) 的数码宽度 —— 字号不同、`close()` 必然不成立，
        // 于是"顶栏没走数码"这条断言**恒真**：把顶栏族改回含数码子集的变异体照样能过。
        // （这个是变异取证抓出来的，不是我读出来的。）
        let w_at = |ctx: &egui::Context, fam: egui::FontFamily, c: char| {
            ctx.fonts_mut(|f| f.glyph_width(&egui::FontId::new(HEADER_SIZE, fam), c))
        };
        let text_fam = || egui::FontFamily::Name(TEXT_FAMILY.into());
        for c in ['3', '0'] {
            let (w_text, w_digit, w_prop) = (
                w_at(&ship, text_fam(), c),
                w_at(&digits, egui::FontFamily::Proportional, c),
                w_at(&ship, egui::FontFamily::Proportional, c),
            );
            assert!(w_digit > 0.0, "数码子集里 {c:?} 没有宽度（子集做坏了？）");
            assert!(
                close(w_prop, w_digit),
                "对照组不成立：主族里的 {c:?} 本该走数码（{w_prop} vs {w_digit}）—— \
                 那样本用例就证明不了顶栏避开了数码体"
            );
            assert!(
                !close(w_text, w_digit),
                "顶栏族里的 {c:?} 宽度仍是数码体的 {w_digit} —— 说明顶栏还在走数码"
            );
        }
    }

    #[test]
    fn a_digit_heavy_name_gets_the_full_budget_not_a_digit_shrunk_one() {
        // 会名不走数码体之后，**额度也必须按那条族算**：数码数字比雅黑宽 39%
        // （同字号 10.608 vs 7.62），拿数码宽度算额度会**过早截断** —— 名字白少显示
        // 几个字，而且看不出来是 bug，就是"短一点"。
        //
        // 判据**不需要知道内部额度**：拿一个**不含数字**的长名字当基准 —— 它在两条族下
        // 量出来一样宽，所以截断结果必然用满额度；带数字那个也应当用满，两者的实际宽度
        // 只该差不到一个字。若带数字那个是按数码宽度算额度，它会短掉几十 pt。
        let Some((_, cjk_bytes)) = local_cjk() else {
            eprintln!("跳过：本机一个中文字体都取不到");
            return;
        };
        let ctx = ctx_with_fonts(build_font_definitions(Some(cjk_bytes)));
        let width_in = |s: &str| {
            ctx.fonts_mut(|f| {
                f.layout_no_wrap(s.to_owned(), text_font(NAME_SIZE), TEXT_COLOR)
                    .size()
                    .x
            })
        };
        // 窄窗逼出截断；用"是名字的前缀"认出会名那个文字块（`▍`/状态词/计时都不是）
        let shown = |name: &str| {
            drawn_texts(&run_frame_at(&[row(name, State::Done, 10)], 260.0))
                .into_iter()
                .map(|(t, _, _)| t)
                .find(|t| {
                    let stem = t.trim_end_matches('…');
                    stem.chars().count() >= 4 && name.starts_with(stem)
                })
                .unwrap_or_else(|| panic!("没认出会名 {name:?}（可能没被截断）"))
        };

        let digits_name = "2026-09-15 v3 示例项目报告排版修复 20260915 第二版";
        let cjk_name = "示例项目报告排版修复第二版第三次修订最终稿再改一版";
        let (a, b) = (shown(digits_name), shown(cjk_name));
        assert!(
            a.ends_with('…') && b.ends_with('…'),
            "构造失败：两个名字都必须在 260pt 下被截断（拿到 {a:?} / {b:?}）"
        );
        let (wa, wb) = (width_in(&a), width_in(&b));
        // 阈值 15 由实测夹出来的（两个方向都量过）：
        //   正确 → {a:"2026-09-15 …"} 113.3pt、{b:"样例项目报告…"} 115.8pt，**差 2.5**
        //   变异（按数码宽度算额度）→ {a:"2026-09-…"} **88.3pt**，差 **27.5**
        // 15 稳稳落在中间。第一版定 30，恰好把 27.5 放过去了 —— 阈值也是要量的。
        assert!(
            (wa - wb).abs() < 15.0,
            "带数字的名字实际占 {wa}pt、纯中文的占 {wb}pt（两者都该接近额度）—— \
             差这么多说明带数字那个是按**数码宽度**算的额度，被过早截断了"
        );
    }

    #[test]
    fn the_session_name_no_longer_goes_through_the_digital_font() {
        // 用户 2026-09-15："会话标题不要用数码的字体，现在里面的数字还是数码格式"。
        //
        // 判据是**渲染出来的宽度**：名字里带数字时（`v3报告`），宽度必须等于"这条族
        // 渲染的宽度"，而不是"数码子集渲染的宽度"—— 同字号下数码数字宽 **39%**
        // （实测 10.608 vs 7.62），两者差得很开，分辨得出来。
        let Some((_, cjk_bytes)) = local_cjk() else {
            eprintln!("跳过：本机一个中文字体都取不到，两条族的宽度无从比");
            return;
        };
        let ctx = ctx_with_fonts(build_font_definitions(Some(cjk_bytes)));
        let name = "v3报告"; // 带数字、也带中文
        let width_in = |fam: egui::FontFamily| {
            ctx.fonts_mut(|f| {
                f.layout_no_wrap(
                    name.to_owned(),
                    egui::FontId::new(NAME_SIZE, fam),
                    TEXT_COLOR,
                )
                .size()
                .x
            })
        };
        let (w_name_fam, w_digit_fam) = (
            width_in(name_family()),
            width_in(egui::FontFamily::Proportional),
        );
        assert!(
            w_digit_fam > w_name_fam + 1.0,
            "前提不成立：两条族下 {name:?} 的宽度几乎一样（{w_digit_fam} vs {w_name_fam}），\
             本用例分辨不出会名走了哪条族"
        );

        let drawn = drawn_texts(&run_frame_at(&[row(name, State::Done, 10)], 360.0));
        let w_drawn = drawn
            .iter()
            .find(|(t, _, _)| t == name)
            .map(|(_, _, w)| *w)
            .unwrap_or_else(|| panic!("没画出会名 {name:?}（可能被截断了？）"));
        assert!(
            (w_drawn - w_name_fam).abs() < 0.5,
            "会名渲染宽度 {w_drawn}；按非数码族算是 {w_name_fam}、按数码族算是 {w_digit_fam} \
             —— 应取前者"
        );
    }

    #[test]
    fn a_gone_host_shows_exited_and_not_the_stale_state_word() {
        // 宿主没了就压过一切：这个会话的真实处境是"它已经不在了"，
        // 而不是它最后一个 hook 事件报的那个状态（那个状态会永远停在那里，不会再有事件）。
        let mut r = row("某会话", State::Working, 60);
        r.host_gone = true;
        let texts: Vec<String> = drawn_texts(&run_frame_at(&[r], 320.0))
            .into_iter()
            .map(|(t, _, _)| t)
            .collect();
        assert!(
            texts.iter().any(|t| t == EXITED_LABEL),
            "必须画出 {EXITED_LABEL}：{texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t == "工作中"),
            "不该再画它最后的状态词：{texts:?}"
        );
        // 会名与计时不受影响（那一行仍然是一行信息）
        assert!(texts.iter().any(|t| t == "某会话"), "会名仍要显示：{texts:?}");
        assert!(texts.iter().any(|t| t == "01:00"), "计时仍要显示：{texts:?}");
    }

    // ---- 窗口拖动 -------------------------------------------------------------
    // 注：**窗口真的被移动**这一步只能人工验收（`StartDrag` 是发给窗口系统的命令）。
    // 这里能自动验证的是它前面的那一环：拖拽手势是否真的落到了拖拽区上
    // —— 用 egui 真实的命中测试跑，指针落在**标签上**也必须拿到 drag_started。
    #[test]
    fn drag_area_receives_a_drag_even_when_the_pointer_is_over_the_content() {
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(320.0, 240.0));
        let frame = |events: Vec<egui::Event>| -> Vec<egui::ViewportCommand> {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            };
            let mut resizing = None;
            let mut full = ctx.run_ui(raw, |ui| {
                handle_window_drag(ui, &mut resizing, &|| Some(egui::Vec2::ZERO));
                ui.label("中文会话名"); // 内容在拖拽区之后注册
            });
            full.textures_delta.clear(); // 不 clear 会 panic（见报告附录）
            full.viewport_output
                .get(&egui::ViewportId::ROOT)
                .map(|o| o.commands.clone())
                .unwrap_or_default()
        };
        let is_drag = |cmds: &Vec<egui::ViewportCommand>| {
            cmds.iter().any(|c| matches!(c, egui::ViewportCommand::StartDrag))
        };

        let pos = egui::pos2(60.0, 30.0); // 落在标签文字上
        frame(vec![]);
        frame(vec![egui::Event::PointerMoved(pos)]);
        let down = frame(vec![egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        }]);

        // 拖拽要越过阈值才算"开始"，所以按下之后还要移动
        let mut cmds = down;
        if !is_drag(&cmds) {
            cmds = frame(vec![egui::Event::PointerMoved(
                pos + egui::vec2(16.0, 0.0),
            )]);
        }
        assert!(is_drag(&cmds), "按住并拖动必须发出 StartDrag：{cmds:?}");
    }

    // ---- 窗口缩放 -------------------------------------------------------------
    // 同拖动：**窗口真的被缩放**只能人工验收（`BeginResize` 是发给窗口系统的命令）。
    // 能自动验证的是前两环：边缘带判得对不对、以及边缘起手时发的是缩放而不是拖动。

    #[test]
    fn resize_direction_covers_corners_and_edges_but_not_the_middle() {
        use egui::viewport::ResizeDirection as Dir;
        let r = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(320.0, 360.0));
        let at = |x: f32, y: f32| resize_direction_at(r, egui::pos2(x, y));

        assert_eq!(at(0.0, 0.0), Some(Dir::NorthWest));
        assert_eq!(at(320.0, 0.0), Some(Dir::NorthEast));
        assert_eq!(at(0.0, 360.0), Some(Dir::SouthWest));
        assert_eq!(at(320.0, 360.0), Some(Dir::SouthEast));
        assert_eq!(at(160.0, 0.0), Some(Dir::North));
        assert_eq!(at(160.0, 360.0), Some(Dir::South));
        assert_eq!(at(0.0, 180.0), Some(Dir::West));
        assert_eq!(at(320.0, 180.0), Some(Dir::East));

        // **中间必须是 `None`** —— 否则整个窗口都成了缩放区，就拖不动了。
        assert_eq!(at(160.0, 180.0), None);
        // 下面的点**跟着 `RESIZE_BORDER` 走**，不写死具体数字：加宽带宽时
        // （2026-09-17 从 6 → 8）写死的那个点会从"不算边缘"变成"算边缘"，
        // 于是这条用例变红、而红的原因与被测行为无关。（实测踩过。）
        let just_inside = RESIZE_BORDER - 1.0;
        let just_outside = RESIZE_BORDER + 1.0;
        assert_eq!(at(just_inside, 180.0), Some(Dir::West), "带宽内侧一点仍算边缘");
        assert_eq!(at(just_outside, 180.0), None, "离边超过一个带宽就不算边缘");
        assert_eq!(at(-40.0, 180.0), None, "离得太远也不算");
    }

    /// **贴边按下不许变成"拖动窗口"**（两者互斥）。
    ///
    /// 这条原来断言的是"发出 `BeginResize`" —— 2026-09-17 改成自己做缩放之后，
    /// 那条命令不再发；但**互斥**这个不变量仍然要守：贴边按下若同时发 `StartDrag`，
    /// 窗口会一边被拖一边被缩。
    #[test]
    fn pressing_at_the_edge_does_not_start_a_window_drag() {
        let cmds = commands_after_drag_at(egui::pos2(1.0, 180.0), egui::vec2(-14.0, 0.0), 320.0, 360.0);
        assert!(
            !cmds.iter().any(|c| matches!(c, egui::ViewportCommand::StartDrag)),
            "贴边按下不该拖动窗口（那条路只给面板中间）：{cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, egui::ViewportCommand::InnerSize(_))),
            "贴边按下应当缩放：{cmds:?}"
        );
    }

    // ---- 布局：右缘基准与四段纵向间距（2026-09-15 第三轮）-------------------------
    //
    // 这一组断言存在的理由，是**上一版一条都没红**：第三轮把四段间距全改了、
    // 又新立了"右对齐到右缘"这条基准，而改完第一次跑测试 **181 条全过**。
    // 那说明这些量此前没有任何东西钉着 —— 界面可以再变回去而没人知道。

    /// 一帧里画出来的每段文字的左右缘。`drawn_texts` 给的是 (文字, 落点, 宽)。
    fn x_span(texts: &[(String, egui::Pos2, f32)], needle: &str) -> (f32, f32) {
        texts
            .iter()
            .find(|(t, _, _)| t == needle)
            .map(|(_, p, w)| (p.x, p.x + w))
            .unwrap_or_else(|| panic!("这一帧里没有画出 {needle:?}"))
    }

    /// 用户 2026-09-15 第三轮裁定："**状态词+计时右对齐到右缘**"。
    ///
    /// 钉的是那条**基准线**：四个宽度下，计时与子代理的**右缘都必须正好落在内容右界上**。
    ///
    /// 只测一个宽度抓不到这个 bug。改前它们是跟在会名后面顺序排的 —— 320pt 下量
    /// "计时右缘 261、内容右界 309"，看着"离边还有 48pt、大概没事"，但那不是基准，
    /// 是**碰巧**：窗口一宽到 520，右缘还是 261，右边整整 248pt（49%）空着。
    /// 四个宽度一起比，"有没有一条不动的基准线"才看得出来。
    #[test]
    fn the_state_word_and_timer_are_flush_with_the_right_edge() {
        for w in [200.0_f32, 220.0, 320.0, 520.0] {
            let mut r = row("示例方法论系统", State::Working, 125);
            r.subagent_count = 12;
            let texts = drawn_texts(&run_frame_at(&[r], w));
            let lim = inner_right(w);
            for needle in ["02:05", "子代理 12"] {
                let (_, right) = x_span(&texts, needle);
                assert!(
                    close(right, lim),
                    "窗口宽 {w}：{needle:?} 的右缘 {right} 必须正好落在内容右界 {lim} 上"
                );
            }
        }
    }

    /// 右组与左组**共用同一条竖直中心线** —— **两个坐标都要钉，"基准"是二维的**。
    ///
    /// ⚠️ 这条是 2026-09-16 复审补的：本轮四条新测试**全部只量 x**，右组的 y 一个断言都没有。
    /// 复审实测两个**确实改变了行为**的变异都是绿的 —— 把 `row_top` 去掉居中（右组上移 3pt）、
    /// 或把 `top` 整体 +7（右组掉到会名行下方），跑全部 52 条 `ui::tests` **照样全过**。
    /// 而 `paint_right` 的 y 是**自己算的**（`row_top + (row_height - 高)/2`），不是 egui 布的局
    /// —— 正是最容易写错、又最不容易被别的测试撞见的一类。视觉上 3pt 就是"状态词比会名高半头"。
    ///
    /// 判据取"两段各自的中心 y 相等"，不是"等于某个算出来的数" —— 后者又会变成自指断言。
    #[test]
    fn the_right_group_shares_the_left_groups_center_line() {
        let mut r = row("x", State::Working, 125);
        r.subagent_count = 12;
        let rects = drawn_rects(&run_frame_at(&[r], 320.0));
        let rect_of = |needle: &str| {
            rects
                .iter()
                .find(|(t, _)| t == needle)
                .map(|(_, r)| *r)
                .unwrap_or_else(|| panic!("没画出 {needle:?}"))
        };
        // 第一行：会名（左）与状态词 / 计时（右）
        let (name, state, timer) = (rect_of("x"), rect_of("工作中"), rect_of("02:05"));
        for (needle, r) in [("工作中", state), ("02:05", timer)] {
            assert!(
                close(r.center().y, name.center().y),
                "第一行：{needle:?} 的中心 y {} 与会名 {} 不共线（差 {}）",
                r.center().y,
                name.center().y,
                r.center().y - name.center().y
            );
        }
        // 第二行：上下文占比（左）与子代理（右）
        let (ctx, sub) = (rect_of("上下文 36%"), rect_of("子代理 12"));
        assert!(
            close(sub.center().y, ctx.center().y),
            "第二行：子代理的中心 y {} 与上下文 {} 不共线（差 {}）",
            sub.center().y,
            ctx.center().y,
            sub.center().y - ctx.center().y
        );
    }

    /// **会话多到面板装不下时，每个会话的两行都真的画出来了**（右缘基准仍然成立）。
    ///
    /// 用**够高的虚拟屏幕**跑，否则夹具自己就把内容裁了 —— 而且裁得**不均匀**
    /// （左组走 `ui.label`、出可见区就不画；右组走 `painter().galley`、照样画），
    /// 见 `run_frame_sized` 的说明。这条用例的存在意义就是**踩住那个默认高度**：
    /// 谁把 `run_frame_sized` 的高度改小，它会红。
    ///
    /// 顺带钉住"每条会话各一条分隔线"（含最后一条）—— 这是本轮的界面事实。
    #[test]
    fn every_session_is_drawn_when_there_are_more_sessions_than_fit() {
        let rows: Vec<view::Row> = (0..4)
            .map(|i| {
                let mut r = row(&format!("会{i}"), State::Done, 10 * (i + 1));
                r.context_pct = Some(0.1 * (i + 1) as f32); // 每行的第二行字各不相同，免得匹配错行
                r
            })
            .collect();
        // 默认 320×360 只装得下 3 个会话，这里给足高度（4 个会话 ≈ 4×94 + 顶栏）。
        let shapes = run_frame_sized(&rows, 320.0, 700.0, &config::Config::default());
        let texts = drawn_texts(&shapes);
        for i in 0..4 {
            for needle in [format!("会{i}"), format!("上下文 {}%", (i + 1) * 10)] {
                assert!(
                    texts.iter().any(|(t, _, _)| *t == needle),
                    "{needle:?} 没画出来（文本：{:?}）",
                    texts.iter().map(|(t, _, _)| t.as_str()).collect::<Vec<_>>()
                );
            }
        }
        let lim = inner_right(320.0);
        for needle in ["00:10", "00:40"] {
            // 第一条与**最后一条**会话的计时都必须贴同一条右缘基准
            let (_, right) = x_span(&texts, needle);
            assert!(close(right, lim), "{needle:?} 右缘 {right} 未落在内容右界 {lim}");
        }
        let rules = shapes
            .iter()
            .filter(|s| matches!(s, egui::Shape::LineSegment { .. }))
            .count();
        assert_eq!(rules, 5, "顶栏下 1 条 + 每个会话各 1 条（含最后一条）");
    }

    /// 右组内部：**状态词在计时左边**，两者之间正好一个 `item_spacing.x`。
    ///
    /// `paint_right` 是**从右往左**画的（先画计时、再画状态词），顺序写反了会静默变成
    /// "计时 工作中" —— 右缘那条断言照样过，因为它只管最右那一段。
    #[test]
    fn in_the_right_group_the_state_word_sits_left_of_the_timer() {
        let texts = drawn_texts(&run_frame_at(&[row("x", State::Working, 125)], 320.0));
        let (state_l, state_r) = x_span(&texts, "工作中");
        let (clock_l, _) = x_span(&texts, "02:05");
        assert!(state_l < clock_l, "状态词必须在计时左边：{state_l} vs {clock_l}");
        assert!(
            close(clock_l - state_r, 8.0),
            "状态词与计时之间应正好一个 item_spacing.x(8pt)，实测 {}",
            clock_l - state_r
        );
    }

    /// **会名不许撞上右组** —— 用最坏情况：最长会名 + 最宽的状态词与计时，
    /// 在**最小窗口宽度**下量。
    ///
    /// 这条是右对齐方案引入的**新风险**：会名由 `name_budget` 截断（额度是**算**出来的），
    /// 而右组由 `paint_right` **画**在固定位置（不参与布局、不会把会名挤走）。
    /// 两边一旦对不上，表现就是长会名**叠在**状态词上面 —— 而不是像改前那样被挤出去。
    /// 所以"不重叠"必须自己断言，不能指望布局兜底。
    ///
    /// ⚠️ **必须把 `host_gone` 那三档一起测**（2026-09-16 复审抓的 Critical）：
    /// 第一版只测了 `State::Working` + 非 gone，而**唯一会让"算的额度"与"画的字"
    /// 分叉的格子恰好就是 `host_gone`** —— 额度按 2 字的 `state_label` 扣、画的是
    /// 3 字的 `EXITED_LABEL`（宽 15pt），于是会名真的叠上去（220pt + 拉丁长名压 5.06pt）。
    /// 教训与本项目别处一致：**"某条不变量有测试守着"这句话，必须连"守的是哪几格"一起说。**
    #[test]
    fn a_long_name_never_runs_into_the_right_group_even_at_the_minimum_width() {
        // 三种宿主已退出的状态（额度按 2 字的「完成/待命/出错」扣、画的是 3 字的「已退出」）
        // + 非 gone 的最宽状态词（「等待确认」4 字，额度与画的是同一个词、不分叉）。
        // 拉丁长名与中文长名各来一个：拉丁字窄，更容易正好把额度填满（余量最薄）。
        let cases = [
            ("计算机基础学习国内视频资源替换", State::Working, false),
            ("abcdefghijklmnopqrstuvwxyz", State::Done, true),
            ("abcdefghijklmnopqrstuvwxyz", State::Idle, true),
            ("计算机基础学习国内视频资源替换", State::Error, true),
        ];
        for (name, state, host_gone) in cases {
            for width in [MIN_INNER_WIDTH, 320.0] {
                let mut r = row(name, state, 3599);
                r.host_gone = host_gone;
                r.subagent_count = 12;
                // 这一行**实际画出来的**状态词（不是从 `state` 另推一个 —— 那正是本用例要防的分叉）。
                let (label, _) = state_text(&r);
                let prefix: String = name.chars().take(3).collect();
                let texts = drawn_texts(&run_frame_at(&[r], width));
                let drawn = texts
                    .iter()
                    .find(|(t, _, _)| t.starts_with(&prefix))
                    .unwrap_or_else(|| panic!("{name:?} 的会名没画出来"))
                    .clone();
                let (label_l, _) = x_span(&texts, label);
                let name_r = drawn.1.x + drawn.2;
                assert!(
                    name_r <= label_l,
                    "窗宽 {width} · {state:?} · host_gone={host_gone}：会名 {name_r} 撞上了 \
                     {label:?} 左缘 {label_l}（压 {}pt，会名画的是 {:?}）",
                    name_r - label_l,
                    drawn.0
                );
                assert!(
                    drawn.0.ends_with('…'),
                    "这么长的会名必须被截断加省略号，实际画的是 {:?}",
                    drawn.0
                );
            }
        }
    }

    /// 造一本"某个会话用了 n 个 token"的账本（真形状的行喂进去，不经手写数字）。
    ///
    /// ⚠️ **时间戳取"现在"，不许写死日期**：账本按**本地日历日**判"是不是今天"，
    /// 而夹具里写死的那个日期一旦过了零点就不再是"今天" ⇒ `add` 返回 false ⇒
    /// 这条断言在**每天跨零点之后必红**。实测踩过：2026-09-17 早上跑测试，
    /// 三条用例一起红，而根因是**昨天写夹具时埋的定时炸弹**，与当时改的东西毫无关系。
    fn ledger_with(session: &str, n: u64) -> usage::Ledger {
        let mut l = usage::Ledger::for_day(chrono::Local::now().date_naive());
        let raw = serde_json::json!({
            "type": "assistant",
            "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "uuid": format!("u-{session}-{n}"),
            "cwd": "D:/p/proj",
            "message": { "usage": { "cache_read_input_tokens": n } }
        })
        .to_string();
        let line = usage::parse_usage_line(&raw).expect("夹具必须是合法 JSON 行");
        assert!(l.add(session, &line), "夹具必须真的被计入");
        l
    }

    /// **T3：顶栏写全机今日总量，行尾写这条会话自己的今日量。**
    ///
    /// 两个数**口径不同、差值是设计的**：行内之和 ≤ 总计（总计还含今天开过、
    /// 现在已关掉的会话，它们不在列表里）。用户 2026-09-14 就知情并接受了这一点。
    #[test]
    fn the_header_shows_the_whole_machines_total_and_the_row_shows_its_own() {
        let mut a = row("会话甲", State::Working, 125);
        a.session_id = "sess-a".into();
        let mut b = row("会话乙", State::Done, 125);
        b.session_id = "sess-b".into();

        let mut ledger = ledger_with("sess-a", 72_000_000);
        // 再来一条**不在列表里**的会话（今天开过、已经关掉）—— 它只进总计
        let extra = usage::parse_usage_line(
            &serde_json::json!({
                // 时间戳同样取"现在"：写死日期会让这条在跨零点后必红（同 `ledger_with` 的注释）
                "type": "assistant",
                "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "uuid": "u-gone", "cwd": "D:/p/proj",
                "message": {"usage": {"cache_read_input_tokens": 93_000_000u64}}
            })
            .to_string(),
        )
        .unwrap();
        assert!(ledger.add("sess-gone", &extra));

        let shapes = run_frame_sized_with_ledger(
            &[a, b],
            320.0,
            400.0,
            // 喂**视图**而不是整本账 —— 与生产路径（`spawn_poller` 里那句 `view()`）一致，
            // 否则测试测的是"另一种对象"，而这两种对象在跨零点/去重上行为并不相同。
            &ledger.view(),
            &config::Config::default(),
        );
        let texts: Vec<String> = drawn_texts(&shapes).into_iter().map(|(t, _, _)| t).collect();
        assert!(
            texts.iter().any(|t| t == "● claude 会话 · 2 个 · 全部 165.0M"),
            "顶栏要写全机今日总量（含已关闭的会话）：{texts:?}"
        );
        assert!(texts.iter().any(|t| t == "72.0M"), "会话甲行尾写它自己的量");
        assert!(texts.iter().any(|t| t == "0"), "会话乙今天没用量，写 0 而不是空着：{texts:?}");
    }

    /// **没扫过账本时不写那个数**（启动头一帧）—— 写 `全部 0` 会把"还不知道"说成"今天没用"。
    #[test]
    fn the_header_omits_the_total_before_the_first_scan() {
        let texts: Vec<String> = drawn_texts(&run_frame_at(&[row("会话甲", State::Working, 125)], 320.0))
            .into_iter()
            .map(|(t, _, _)| t)
            .collect();
        assert!(
            texts.iter().any(|t| t == "● claude 会话 · 1 个"),
            "没有账本时顶栏只写会话数：{texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("全部")),
            "不该出现「全部 0」：{texts:?}"
        );
    }

    /// 第二行的 token 数**不许撞上右侧的 `子代理 N`** —— 最窄、最长数字的那一格。
    ///
    /// 第二行左段是 `上下文 N%` + token 数（都是左对齐），右段是 `子代理 N`（贴右缘）。
    /// 两者之间的余量是本行唯一会被数字撑破的东西，所以拿**最坏组合**来钉。
    #[test]
    fn the_token_number_never_runs_into_the_subagent_count() {
        let mut r = row("会话甲", State::Working, 3599); // `59:59` —— MM:SS 档能到的最长值
        r.session_id = "sess-a".into();
        r.subagent_count = 12;
        r.context_pct = Some(1.0); // `上下文 100%` 是最宽的那种
        let ledger = ledger_with("sess-a", 165_000_000);
        for w in [MIN_INNER_WIDTH, 320.0] {
            let shapes = run_frame_sized_with_ledger(&[r.clone()], w, 400.0, &ledger.view(), &config::Config::default());
            let rects = drawn_rects(&shapes);
            let rect_of = |n: &str| {
                rects
                    .iter()
                    .find(|(t, _)| t == n)
                    .map(|(_, r)| *r)
                    .unwrap_or_else(|| panic!("窗宽 {w} 没画出 {n:?}"))
            };
            let (tok, sub) = (rect_of("165.0M"), rect_of("子代理 12"));
            assert!(
                tok.right() <= sub.left(),
                "窗宽 {w}：token 数右缘 {} 撞上了子代理左缘 {}",
                tok.right(),
                sub.left()
            );
        }
    }

    /// 一帧里"每一行的色条颜色"，按行配对：`(行里画出的会名, 色条的 RGB)`。
    ///
    /// 色条与会名在同一行、y 相同，所以按 y 配对 —— 这样测试不必假设行的顺序
    /// （顺序本身正是要有独立用例去验的东西）。
    fn bar_colors_by_name(shapes: &[egui::Shape]) -> Vec<(String, egui::Color32)> {
        let mut bars: Vec<(f32, egui::Color32)> = Vec::new();
        let mut names: Vec<(f32, f32, String)> = Vec::new();
        for s in shapes {
            match s {
                // 色块 = **矩形**（`Shape::Rect`，不是字形）—— 它的宽度必须与 `BAR_WIDTH` 一致，
                // 否则说明画的不是我们以为的那个东西。
                egui::Shape::Rect(r) => {
                    if (r.rect.width() - BAR_WIDTH).abs() < 0.01
                        && (r.rect.height() - BAR_HEIGHT).abs() < 0.01
                    {
                        bars.push((r.rect.center().y, r.fill));
                    }
                }
                egui::Shape::Text(t) => {
                    let text = t.galley.text().to_owned();
                    if !text.is_empty() && !text.starts_with('●') && !text.starts_with("上下文") {
                        // ⚠️ 用**垂直中心**配对：色块是居中画的（`from_center_size`），
                        // 而字的 `pos` 是**顶**（galley 原点）。拿顶去撞中心会差半个行高，
                        // 实测差 11pt —— 配对全部失败，返回一个空列表。
                        names.push((t.pos.y + t.galley.size().y / 2.0, t.pos.x, text));
                    }
                }
                _ => {}
            }
        }
        // ⚠️ y 的比较**不能用 0.5 这种紧容差**：色条走默认族（17pt → galley 高 19），
        // 会名走会名族（17pt → 22），两者在 `Align::Center` 里居中的基准不同，
        // 实测差约 1.5pt。取同一行最近的那根色条即可（行与行的间距是几十 pt，不会配错）。
        // **一根色块只配一个名字**：同一行里还有计时、状态词、token 等文字，它们与色块同高
        // —— 全都配一遍会让"一行"在结果里出现三四次（第一版就是这么写的，
        // 于是"可见会话两两不同色"的断言被自己的重复项搞红）。
        // 取**行内最靠左**的那段文字（会名在左端），它才是这条会话的名字。
        bars.iter()
            .filter_map(|(by, c)| {
                names
                    .iter()
                    .filter(|(ny, _, _)| (ny - by).abs() < 8.0)
                    .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                    .map(|(_, _, name)| (name.clone(), *c))
            })
            .collect()
    }

    /// **颜色分配的两条硬保证**：同时可见的会话**互不同色**；会话集合不变时**颜色不变**。
    ///
    /// 用户要的是"一眼分得开"（"色条区别度不高" → "就用红黄蓝绿"），
    /// 所以"不重色"是硬保证；而"哈希色优先"是为了稳定 ——
    /// 用户刚记住"红块那条是调研"，不该因为另一个会话结束就换色。
    #[test]
    fn visible_sessions_get_distinct_colours_and_keep_them_while_the_set_is_unchanged() {
        let mk = |id: &str| {
            let mut r = row("会话", State::Working, 10);
            r.session_id = id.into();
            r
        };
        // ① 四条（= 色板长度 = 用户声明的上限）→ 四种颜色，两两不同
        let rows: Vec<view::Row> = ["a", "b", "c", "d"].iter().map(|i| mk(i)).collect();
        let cs = assign_colors(&rows);
        let uniq: std::collections::HashSet<_> = cs.iter().map(|c| (c.r(), c.g(), c.b())).collect();
        assert_eq!(uniq.len(), 4, "四条会话必须拿到四种颜色（实测 {cs:?}）");

        // ② 同一条会话在集合不变时颜色不变（换个顺序也一样 —— 颜色认 id，不认行号）
        let rev: Vec<view::Row> = ["d", "c", "b", "a"].iter().map(|i| mk(i)).collect();
        let cs2 = assign_colors(&rev);
        for (id, c) in ["a", "b", "c", "d"].iter().zip(cs.iter()) {
            let idx = rev.iter().position(|r| r.session_id == *id).unwrap();
            assert_eq!(
                cs2[idx], *c,
                "会话 {id} 换了位置就换了颜色 —— 说明分配认了行号"
            );
        }

        // ③ 超过色板长度就从头上重复（已知代价，写在这里免得被当成 bug）
        let many: Vec<view::Row> = (0..8).map(|i| mk(&format!("s{i}"))).collect();
        let cm = assign_colors(&many);
        let uniq_m: std::collections::HashSet<_> = cm.iter().map(|c| (c.r(), c.g(), c.b())).collect();
        assert_eq!(uniq_m.len(), 4, "超过 4 条只能重复用色板（实测 {uniq_m:?}）");
    }

    /// **色板每一色都要过白底 AA**（对比度 ≥ 4.5:1）。
    ///
    /// 这条不是审美偏好，是本仓对每个上屏颜色的既有要求（`EXITED_COLOR` / `state_color`
    /// 的注释里都标了实测对比度）。用 WCAG 的公式现算，而不是把注释里的数字抄一遍 ——
    /// 抄的数字会在有人调色时立刻过期，而注释不会自己报错。
    #[test]
    fn every_session_colour_passes_contrast_on_white() {
        fn lum(c: egui::Color32) -> f64 {
            fn ch(v: u8) -> f64 {
                let v = v as f64 / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            }
            0.2126 * ch(c.r()) + 0.7152 * ch(c.g()) + 0.0722 * ch(c.b())
        }
        for c in SESSION_COLORS {
            let ratio = 1.05 / (lum(c) + 0.05);
            assert!(
                ratio >= 4.5,
                "色 #{:02x}{:02x}{:02x} 的白底对比度只有 {ratio:.2}（AA 要 ≥ 4.5）",
                c.r(), c.g(), c.b()
            );
        }
    }

    /// **颜色只由 `session_id` 决定**：与状态、名字、行序都无关，且跨进程稳定。
    #[test]
    fn the_bar_colour_identifies_the_session_not_its_state_or_position() {
        // ① 同一个 id 永远同一个色（换个进程也一样 —— 哈希是确定性的，不用 DefaultHasher）
        let a = hash_color("sess-a");
        assert_eq!(a, hash_color("sess-a"));
        assert_eq!(
            hash_color("sess-a"),
            hash_color(&"sess-a".to_string()),
            "同一串字节必须得到同一色（跨调用/跨进程稳定）"
        );

        // ② **反向对照**：状态不影响色条 —— 同一会话、不同状态，颜色必须一样。
        //    改之前色条走 state_color，这条会红（那正是用户抱怨的"区别度不高"）。
        let mut done = row("会话甲", State::Done, 10);
        done.session_id = "sess-a".into();
        let mut working = row("会话甲", State::Working, 10);
        working.session_id = "sess-a".into();
        let c1 = bar_colors_by_name(&run_frame_at(&[done], 320.0))[0].1;
        let c2 = bar_colors_by_name(&run_frame_at(&[working], 320.0))[0].1;
        assert_eq!(c1, a, "色条要用会话色，不是状态色");
        assert_eq!(c2, a, "换状态不该换颜色");

        // ③ **两条会话（即使状态相同）颜色应当不同** —— 这正是用户要的"区别度"。
        //    挑两个哈希落在不同桶的 id（找不到就说明色板/哈希退化了）。
        let ids = ["sess-a", "sess-b", "sess-c", "sess-d"];
        let colours: std::collections::HashSet<_> = ids.iter().map(|i| hash_color(i)).collect();
        assert!(
            colours.len() >= 3,
            "四个 id 至少该落在 3 个不同的色上（实测 {:?}）",
            ids.iter().map(|i| hash_color(i)).collect::<Vec<_>>()
        );

        // ④ 颜色与**行序**无关：把两行换个顺序渲染，每行仍然拿自己的色。
        let mk = |id: &str, name: &str| {
            let mut r = row(name, State::Working, 10);
            r.session_id = id.into();
            r
        };
        let two = |rows: Vec<view::Row>| {
            bar_colors_by_name(&run_frame_sized(&rows, 320.0, 400.0, &config::Config::default()))
        };
        let ab = two(vec![mk("sess-a", "会话甲"), mk("sess-b", "会话乙")]);
        let ba = two(vec![mk("sess-b", "会话乙"), mk("sess-a", "会话甲")]);
        let find = |v: &[(String, egui::Color32)], n: &str| {
            v.iter().find(|(t, _)| t == n).unwrap_or_else(|| panic!("没画出 {n}")).1
        };
        assert_eq!(find(&ab, "会话甲"), find(&ba, "会话甲"), "换顺序不该换颜色");
        assert_eq!(find(&ab, "会话乙"), find(&ba, "会话乙"));
        assert_ne!(find(&ab, "会话甲"), find(&ab, "会话乙"), "两条会话该有不同颜色");
    }

    /// **位置记忆的判据**（纯函数）。真窗口的拖动只能人工验收，所以这条判据必须自己钉住。
    ///
    /// 用户 2026-09-16 批准做这件事。改之前 `window_pos` 是**死配置**：有字段、无读者。
    #[test]
    fn the_window_position_is_saved_only_when_it_settles_and_really_moved() {
        let saved = [100.0_f32, 100.0];
        // 还在拖（没停稳）—— 哪怕位置已经变了也不写：一次拖拽会写几十次
        assert!(!should_save_pos([300.0, 400.0], saved, 0.0));
        assert!(!should_save_pos([300.0, 400.0], saved, 0.99));
        // 停稳了但没动 —— 不写（否则每次启动都会把配置重写一遍）
        assert!(!should_save_pos(saved, saved, 5.0));
        assert!(!should_save_pos([100.5, 100.0], saved, 5.0), "半像素抖动不算动");
        assert!(!should_save_pos([101.0, 100.0], saved, 5.0), "正好一个 epsilon 不算动（判据是严格大于）");
        // 停稳 + 真的动了 —— 写
        assert!(should_save_pos([101.5, 100.0], saved, 1.0));
        assert!(should_save_pos([100.0, 260.0], saved, 1.0), "只动一个轴也算");
        // 坐标不可信时不写 —— 一个落在屏幕外的坐标会让无边框窗口再也找不回来
        assert!(!should_save_pos([f32::NAN, 100.0], saved, 5.0));
        assert!(!should_save_pos([1.0e9, 100.0], saved, 5.0));
    }

    /// 把一帧里的**线段**分成竖与横两组（分隔线也是 `LineSegment`，必须能区分）。
    ///
    /// 判据是"两个端点的 x 是否相等" —— 折线的两段必然一段竖直、一段水平，
    /// 而 `rule()` 画的分隔线是纯水平的。**斜线两边都不落**，于是它也算一种可断言的特征。
    fn line_segments(shapes: &[egui::Shape]) -> (Vec<((f32, f32), (f32, f32))>, Vec<((f32, f32), (f32, f32))>) {
        let mut vertical = Vec::new();
        let mut horizontal = Vec::new();
        for s in shapes {
            if let egui::Shape::LineSegment { points, .. } = s {
                let (a, b) = (points[0], points[1]);
                if (a.x - b.x).abs() < 0.01 {
                    vertical.push(((a.x, a.y), (b.x, b.y)));
                } else if (a.y - b.y).abs() < 0.01 {
                    horizontal.push(((a.x, a.y), (b.x, b.y)));
                }
            }
        }
        (vertical, horizontal)
    }

    /// **子代理连线**：从「状态词」的**中点**垂下来，到子代理那一行的中心高度**右拐**，
    /// 停在「子代理 N」左缘外 2pt。用户 2026-09-16 裁定的 ∟。
    ///
    /// 端点全部**对着画出来的文字的框**断言（不是对着常量）—— 这条线要"指得准"，
    /// 而"准"的定义就是"落在那两段文字上"。
    #[test]
    fn a_subagent_gets_a_right_angle_link_to_its_state_word() {
        let mut r = row("web-lab", State::Working, 12500); // 02:05
        r.subagent_count = 2;
        for w in [320.0_f32, MIN_INNER_WIDTH] {
            let shapes = run_frame_at(&[r.clone()], w);
            let rects = drawn_rects(&shapes);
            let rect_of = |n: &str| {
                rects
                    .iter()
                    .find(|(t, _)| t == n)
                    .map(|(_, r)| *r)
                    .unwrap_or_else(|| panic!("窗宽 {w} 没画出 {n:?}"))
            };
            let (state, sub) = (rect_of("工作中"), rect_of("子代理 2"));
            let (vertical, _) = line_segments(&shapes);
            assert_eq!(
                vertical.len(),
                1,
                "窗宽 {w}：应当正好一条竖段（实测 {vertical:?}）"
            );
            let (top, bottom) = vertical[0];
            // 竖段：与状态词**同一个 x 中心**，上端在状态词下缘外、下端落在子代理的行中心
            assert!(
                close(top.0, state.center().x),
                "窗宽 {w}：竖段的 x {} 应当是状态词中心 {}",
                top.0,
                state.center().x
            );
            assert!(
                close(top.1, state.bottom() + 2.0),
                "窗宽 {w}：竖段上端 {} 应贴在状态词下缘外 2pt（{}）",
                top.1,
                state.bottom() + 2.0
            );
            assert!(
                close(bottom.1, sub.center().y),
                "窗宽 {w}：竖段下端 {} 应落在子代理行中心 {}",
                bottom.1,
                sub.center().y
            );
            // 横段：与竖段共端点，右端停在子代理左缘外 2pt
            let (hx1, hy1, hx2) = {
                let (_, h) = line_segments(&shapes); // ← 第二个才是横段，别拿错了
                let seg = h
                    .iter()
                    .find(|((_, y1), (_, y2))| close(*y1, bottom.1) && close(*y2, bottom.1))
                    .unwrap_or_else(|| panic!("窗宽 {w}：没找到那条横段（实测 {h:?}）"));
                let (a, b) = (seg.0, seg.1);
                (a.0, a.1, if a.0 > b.0 { a.0 } else { b.0 })
            };
            assert!(
                close(hy1, sub.center().y),
                "窗宽 {w}：横段 y {} 应在子代理行中心 {}",
                hy1,
                sub.center().y
            );
            assert!(
                close(hx2, sub.left() - 2.0),
                "窗宽 {w}：横段右端 {} 应停在子代理左缘外 2pt（{}）",
                hx2,
                sub.left() - 2.0
            );
            assert!(close(hx1, top.0), "窗宽 {w}：横段左端必须接在竖段下端");
            // 不越界：两端都必须落在内容区里（最小宽度那档最紧）
            let (left_lim, right_lim) = (11.0, inner_right(w));
            for (x, y) in [(top.0, top.1), (bottom.0, bottom.1), (hx1, hy1), (hx2, hy1)] {
                assert!(
                    x >= left_lim && x <= right_lim,
                    "窗宽 {w}：折线的点 ({x}, {y}) 越出内容区 [{left_lim}, {right_lim}]"
                );
            }
        }
    }

    /// **反向对照**：没有子代理时**一条竖线都不该有**。
    ///
    /// 用户裁定的是"**有子代理就画**"，不是"总是画"。少了这条，把触发条件写成无条件的
    /// 变异体照样绿 —— 而界面上会多出一条指向空气的折线。
    #[test]
    fn no_subagent_means_no_link() {
        // ⚠️ **横段也要数**：变异取证实测过 —— 只数竖段的话，"无条件画"那个变异体
        // 虽然画不出竖段（起止点被 `y_mid > y_top` 挡住），却会**多画一条横线**，
        // 而只数竖段的断言照样绿。判据改成"横线总数 = 分隔线数 +（有子代理 ? 1 : 0）"。
        let plain = row("web-lab", State::Working, 125);
        let shapes = run_frame_at(&[plain], 320.0);
        let (vertical, horizontal) = line_segments(&shapes);
        assert!(
            vertical.is_empty(),
            "没有子代理时不该有竖段（实测 {vertical:?}）"
        );
        assert_eq!(
            horizontal.len(),
            2, // 顶栏下 1 条 + 这条会话 1 条；多出来的那一条就是"指向空气的折线"
            "没有子代理时横线只该有分隔线（实测 {horizontal:?}）"
        );

        // 正面对照：把子代理加上，两条断言都该往"多一条竖段、多一条横段"走 ——
        // 证明上面那两个数不是空话。
        let mut with = row("web-lab", State::Working, 125);
        with.subagent_count = 1;
        let (vertical, horizontal) = line_segments(&run_frame_at(&[with], 320.0));
        assert_eq!(vertical.len(), 1, "有子代理时必须画出一条竖段（对照）");
        assert_eq!(horizontal.len(), 3, "有子代理时横线 = 2 条分隔线 + 1 条折线横段（对照）");
    }

    /// 一帧里每段文字的**外框**（落点 + galley 实际尺寸）。
    ///
    /// 不拿 `Fonts::row_height(字号)` 去估高度 —— 实测那样会差 0.44pt（galley 的尺寸
    /// 走过取整），而"间距必须是 14pt"这种断言差 0.4 就没意义了。**量画出来的那个矩形。**
    fn drawn_rects(shapes: &[egui::Shape]) -> Vec<(String, egui::Rect)> {
        shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::Text(t) => Some((
                    t.galley.text().to_owned(),
                    egui::Rect::from_min_size(t.pos, t.galley.size()),
                )),
                _ => None,
            })
            .collect()
    }

    /// 四段纵向间距，各自对着**用户裁定过的那个数**：
    /// 顶栏→第一条会话 **16** · 第一行→第二行 **14** · 第二行→分隔线 **8**。
    ///
    /// 这三条钉的都是**界面上量到的空隙**，不是那三个常量 —— 常量只是旋钮。
    /// 改前它们分别是 6 / 8 / 11：第二行离**下面的线**比离**它注释的那一行**还远，
    /// 而顶栏与正文之间的空隙比正文内部还紧 —— 两处层级都是倒的。
    /// （会话间距 58pt 那条旧裁定由 `rows_are_spaced_twice_as_far_apart_as_before` 守着。）
    ///
    /// ⚠️ **两条会话都要量**（2026-09-16 复审补的）：第一版两行用的是同一串
    /// `上下文 36%`，`find` 静默取到**第一行**，于是 14 / 8 只在第 0 个会话上量过 ——
    /// 将来若出现"只有第二次循环才走的分支"（例如按 `host_gone` 换间距），它照样绿。
    /// 现在两行的 `context_pct` 不同，按各自的字取矩形。
    #[test]
    fn the_four_vertical_gaps_are_the_ruled_ones() {
        let mut second = row("BBB", State::Done, 20);
        second.context_pct = Some(0.41); // 与第一行的 `上下文 36%` 区分开，免得匹配错行
        let rows = vec![row("AAA", State::Done, 10), second];
        let shapes = run_frame_at(&rows, 320.0);
        let rects = drawn_rects(&shapes);
        let rect_of = |needle: &str| {
            rects
                .iter()
                .find(|(t, _)| t == needle)
                .map(|(_, r)| *r)
                .unwrap_or_else(|| panic!("没画出 {needle:?}"))
        };
        // 分隔线是 `Shape::LineSegment`（零占位），不是文字，得单独捞。
        let rules: Vec<f32> = shapes
            .iter()
            .filter_map(|s| match s {
                egui::Shape::LineSegment { points, .. } => Some(points[0].y),
                _ => None,
            })
            .collect();
        assert_eq!(rules.len(), 3, "顶栏下 + 每个会话各一条：{rules:?}");

        let header = rect_of("● claude 会话 · 2 个");

        // ⚠️ 这三个数**写字面量，不写常量**。写成 `close(gap, ROW_LINE_GAP)` 看着更
        // "不重复"，但那样断言是**空的**：把常量 14 改成 8，界面真的变成 8 之后
        // 两边一起动，照样绿 —— 变异取证实测过（E/F 两条就是这么漏过去的）。
        // 常数只是旋钮，**被裁定的是界面上那个数**。
        // 顶栏自己与它的分隔线之间**不**该拉开 —— 那条线属于顶栏（阈值也是 0，不是"小于 16"：
        // 单边宽阈值会把"线浮到离顶栏 15pt"这种走样放过去，复审实测过）。
        let gap_header = rect_of("AAA").top() - rules[0];
        assert!(close(gap_header, 16.0), "顶栏→第一条会话应为 16pt，实测 {gap_header}");
        assert!(
            close(rules[0] - header.bottom(), 0.0),
            "分隔线应该**紧贴**顶栏（0pt），实测 {}",
            rules[0] - header.bottom()
        );

        // 每一段都对**它自己那一条会话**量一遍。
        for (i, (name, ctx_text, rule_y)) in [
            ("AAA", "上下文 36%", rules[1]),
            ("BBB", "上下文 41%", rules[2]),
        ]
        .into_iter()
        .enumerate()
        {
            let name_rect = rect_of(name);
            let ctx = rect_of(ctx_text);
            let gap_line = ctx.top() - name_rect.bottom();
            let gap_bottom = rule_y - ctx.bottom();
            assert!(
                close(gap_line, 14.0),
                "第 {i} 个会话：第一行→第二行应为 14pt，实测 {gap_line}"
            );
            assert!(
                close(gap_bottom, 8.0),
                "第 {i} 个会话：第二行→分隔线应为 8pt，实测 {gap_bottom}"
            );
            assert!(
                gap_line > gap_bottom,
                "第 {i} 个会话：第二行必须离**上面那行**比离**下面的线**更远\
                 （{gap_line} vs {gap_bottom}），否则它读起来是不属于任何一边的掉队注脚"
            );
        }
    }

    /// **最小宽度那张退化表（170 消失 / 190 `…` / 200 `示…` / 220 `示例…`）逐格钉住。**
    ///
    /// 这张表原先只有 220 那一格被测着，而 `MIN_INNER_WIDTH` 的注释却说"这条测试守着它"
    /// （2026-09-16 复审抓的）。三格没测 = 字体一换或额度公式一动，这张表就会悄悄漂走，
    /// 而"取某个数"变成一个没有理由的数。
    ///
    /// ⚠️ **这张表是「5 个字符的计时」那一档**（`59:59`），它**不是** `MIN_INNER_WIDTH`
    /// 的全部依据 —— 6 个字符（`999:59`）会把每一格都推后一档，那一条由
    /// [`Self::a_long_timer_does_not_steal_the_second_name_character`] 守着，
    /// 而 222 这个下限是**按 6 个字符那一档**取的（见 `MIN_INNER_WIDTH` 的注释）。
    ///
    /// 用**最坏情况** fixture：最长会名 + 最宽的状态词 + **这一档里最长的计时** + 两位子代理数。
    /// 期望值是**字面量**（复审在真帧上逐格量过）。
    ///
    /// 📌 计时用 `3599`（`59:59`，`MM:SS` 能到的最长）而不是 "随便一个两位数分钟"：
    /// 2026-09-21 之前这里是 `3725`（改前渲染成 `62:05`）；计时改成满 1 小时换 `H:MM` 之后
    /// 同一个 `3725` 只剩 `1:02`（4 字符），**额度凭空多出一格 ⇒ 四格期望值全部前移**。
    /// 换成这一档真正的最坏值，既保住原表的裁决力，也让夹具不再依赖一个"碰巧够长"的数。
    #[test]
    fn the_minimum_width_is_the_smallest_width_that_still_shows_two_name_characters() {
        for (width, expected) in [(170.0, ""), (190.0, "…"), (200.0, "示…"), (220.0, "示例…")] {
            let mut r = row("示例方法论系统", State::Waiting, 3599);
            r.subagent_count = 12;
            let texts = drawn_texts(&run_frame_at(&[r], width));
            // 会名要么是空串（整段不画），要么是 `示…` 这种被截断的形态。
            let drawn = texts
                .iter()
                .find(|(t, _, _)| t.starts_with('示') || t == "…")
                .map(|(t, _, _)| t.clone())
                .unwrap_or_default();
            assert_eq!(
                drawn, expected,
                "窗宽 {width}：会名应画成 {expected:?}，实际 {drawn:?}"
            );
        }
    }

    /// **计时更长的那一档**（6 个字符：`H:MM` 到 100 小时为止 ⇒ `999:59`，比 `MM:SS` 档
    /// 多一位数 ⇒ 会名的额度少一位数）。钉住"**`MIN_INNER_WIDTH` 恰好是这一档下
    /// 第一个给得出两个字会名的宽度**"。
    ///
    /// 为什么单开一条：2026-09-21 之前**这一档一格都没测**，于是"到最窄宽度会名还有两个字"
    /// 这句话对一个完全真实的情形（会话开着十几小时 / 几天）是假的 —— 实测只有 `示…` 一个字。
    /// 这正是本仓反复吃的那类亏：**守卫用例只测了"不变量恰好成立"的那一格**。
    /// 当时真机上抓到一行 `8368:11`（开了 5.8 天的会话）把这个洞坐实了。
    ///
    /// 断言写法是"**搜出第一个够宽的**再与常量比"，不是"在 `MIN_INNER_WIDTH` 上断言两个字" ——
    /// 后者在字体变窄之后照样绿（常量就该跟着变小，而它不会自己变）。
    #[test]
    fn a_long_timer_does_not_steal_the_second_name_character() {
        // 3_599_940s = 999 小时 59 分 = `999:59`，**6 字符档能到的最长值**（≈41.7 天）。
        let name_at = |width: f32| -> String {
            let mut r = row("示例方法论系统", State::Waiting, 3_599_940);
            r.subagent_count = 12;
            drawn_texts(&run_frame_at(&[r], width))
                .into_iter()
                .find(|(t, _, _)| t.starts_with('示') || t.as_str() == "…")
                .map(|(t, _, _)| t)
                .unwrap_or_default()
        };
        // 反向对照：**220 在长计时下只能给一个字**。没有这一格，"第一个够宽的就是
        // MIN_INNER_WIDTH"在整体变窄/变小之后会给出一个**更小**的第一个宽度，
        // 断言红得莫名其妙；有了它，失败信息同时说清了"哪一档在漂"。
        assert_eq!(
            name_at(220.0),
            "示…",
            "6 字符计时（999:59）下 220pt 本来就该退化成一个字 —— 这条正是上调的依据"
        );
        let first_two_char = (200..=260)
            .find(|w| name_at(*w as f32).chars().filter(|c| *c != '…').count() >= 2)
            .expect("260pt 以内必须存在一个能给两个字会名的宽度") as f32;
        assert_eq!(
            first_two_char, MIN_INNER_WIDTH,
            "MIN_INNER_WIDTH 必须是**长计时**下第一个给得出两个字会名的宽度"
        );
    }

    /// **分隔线的颜色必须来自 [`RULE_COLOR`]，不许跟着宿主主题跑。**
    ///
    /// 来历（2026-09-21，遗留 #3）：那条线原先由 `egui::Separator` 画，描边取自
    /// `visuals.noninteractive.bg_stroke` —— 浅色主题 `gray(190)` = `#BEBEBE`、
    /// **深色主题 `gray(60)` = `#3C3C3C`**（画在纯白面板上反而**更重**）。
    /// 于是**同一个 exe 在两台机器上画出来的线不一样**，而这是全项目唯一一处在跟着系统跑：
    /// 面板底色、每个字、每根折线都是自己给的色值。
    ///
    /// ⚠️ 写法上必须**两边都拨**：只在浅色主题下断言的话，修复前那版画出来的正好也是
    /// `#BEBEBE` ⇒ 断言**修复前后一样绿**（本仓最恨的空断言）。**深色那一趟就是反向对照**，
    /// 它单独盯住"有没有谁又去读主题了"。
    #[test]
    fn the_separator_colour_comes_from_our_constant_not_the_host_theme() {
        // 夹具里**没有子代理**（`row()` 默认 `subagent_count: 0`）⇒ 一帧里的斜线一根都没有，
        // 于是"水平段"只剩分隔线。子代理折线的横段也是水平的，夹具一改就会混进来 ——
        // 所以下面按根数断言（多了就是夹具变了，不是线画错了）。
        let horizontal_lines = |theme| -> Vec<(egui::Stroke, f32)> {
            run_frame_with_theme(&[row("样例项目", State::Working, 72)], theme)
                .iter()
                .filter_map(|s| match s {
                    egui::Shape::LineSegment { points, stroke }
                        if points[0].y == points[1].y =>
                    {
                        Some((*stroke, points[0].y))
                    }
                    _ => None,
                })
                .collect()
        };

        let light = horizontal_lines(egui::Theme::Light);
        let dark = horizontal_lines(egui::Theme::Dark);

        // 存在性：顶栏下 1 条 + 一条会话后面 1 条。**换成自己画之后，少画一条不会有别的东西报错**
        // （原来那个 `ui.add(rule())` 至少还占着一次 `Response`），所以这条必须留着。
        assert_eq!(light.len(), 2, "顶栏下一条 + 会话间一条；实际 {light:?}");
        for (stroke, y) in &light {
            assert_eq!(
                stroke.color, RULE_COLOR,
                "y={y} 那条线的颜色必须来自 `RULE_COLOR`，实际是 {:?}",
                stroke.color
            );
            assert_eq!(stroke.width, RULE_WIDTH, "y={y} 那条线的线宽也必须是显式的");
        }
        // 反向对照：**换主题，画出来必须一模一样**。修复前这条会红
        // （深色主题给 `gray(60)` ≠ 浅色主题的 `gray(190)`）。
        assert_eq!(
            dark, light,
            "分隔线不许跟着宿主主题变 —— 深色主题下画出来的与浅色主题下必须逐点相同"
        );
    }

    /// **首轮扫描还没落地时，不许说"0 个"、也不许说"没有活跃会话"**（遗留 #6）。
    ///
    /// 首轮要读完整份 transcript（本机实测 5 个大会话 88 MB → 730 ms），窗口开出来头一秒
    /// 快照还是 `Default`（空行 + 空账本）。那一刻的"空"是**还不知道**，不是"确实没有" ——
    /// 原样说出来就变成"启动先亮一句错的，一秒后跳成真话"。
    /// 这与顶栏对 `ledger.day()` 的既有口径是同一条规矩：**读不到就少说一句，不编数**。
    #[test]
    fn before_the_first_scan_lands_the_panel_does_not_claim_zero_sessions() {
        // 还没落地：标题在，**一个数都不报**。
        assert_eq!(
            painted_texts(&run_frame_before_the_first_scan(&[])),
            "● claude 会话",
            "首轮扫描没落地时只能说标题 —— 说「0 个」是把「还不知道」讲成了「没有」"
        );
        // 还没落地但**已经有行**？那不是真实情形（同一把锁里出的），不必照顾。

        // 反向对照：扫过了、确实没有 ⇒ **两句话都要说**。
        // 少了这一格，"什么都不画"也能让上面那条绿。
        let empty = painted_texts(&run_frame(&[]));
        assert!(
            empty.contains("0 个"),
            "扫过之后确实没有会话，就该报 0 个；实际 {empty:?}"
        );
        assert!(
            empty.contains("没有活跃会话"),
            "扫过之后确实没有会话，就该说「没有活跃会话」；实际 {empty:?}"
        );
    }

    /// **计时的两档格式与切换点**（用户 2026-09-21 裁定：满 1 小时从 `MM:SS` 换 `H:MM`）。
    ///
    /// 每一条都是**整点边界**：`59:59 → 1:00` 是切换那一瞬（数字会"往回跳"，但那是时钟
    /// 整点进位、谁都认得 —— 这正是选 1 小时而不是 `99:59` 当切换点的理由），
    /// `9:59 → 10:00` / `99:59 → 100:00` 是**字符串变长**那一瞬（每多一位，会名的额度就少 12pt）。
    ///
    /// 最后一条钉的是遗留 #4 的**病根**：改前 `8368:11` 这种数会出现，改后同样的时长是
    /// `139:28`（既短又能读懂）。夹具里的 `502_091` 就是真机截图那一行。
    #[test]
    fn the_timer_switches_to_hours_at_the_hour_mark() {
        for (secs, expected) in [
            (0_i64, "00:00"),
            (59, "00:59"),
            (600, "10:00"),
            (3598, "59:58"),
            (3599, "59:59"),      // MM:SS 档的**最长值**（5 字符）
            (3600, "1:00"),       // ← 切换点：整 1 小时
            (3661, "1:01"),
            (35_994, "9:59"),
            (36_000, "10:00"),    // 字符串开始变长（5 字符）
            (3_599_940, "999:59"),// 6 字符档的最长值（≈41.7 天）
            (502_091, "139:28"),  // 真机那一行：5.8 天的会话
        ] {
            assert_eq!(
                fmt_elapsed(secs),
                expected,
                "{secs}s 应显示成 {expected:?}"
            );
        }
        // 上界只是被推远、没有被消灭：再往上还是会变长。这一条**故意钉住"它会变长"**，
        // 免得有人读上面的表以为 `H:MM` 恒为 5 个字符。
        assert_eq!(fmt_elapsed(360_000_000), "100000:00");
    }
}
