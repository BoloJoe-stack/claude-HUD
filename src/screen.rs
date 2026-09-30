//! 可见桌面：**显示器矩形**（Win32 直调 + 一个纯函数）。
//!
//! ## 为什么需要
//!
//! 挂件是**无边框**窗口（没有标题栏）—— 所以"跑到屏幕外"对它不是麻烦，是**消失**：
//! 有标题栏的窗口拖得回来，没有的拖不回来。2026-09-30 就是这么丢的：`config.json` 里
//! `window_pos = [-484, 603]`，而当天是**单屏** `0..2560`，整窗落在左边界外；此后每次
//! 启动都按配置回到屏幕外。用户看到的只是"hud 又用不了了" —— 进程好好活着、也在轮询，
//! 只是画在**看不见的地方**（截图核对过：面板、会话、计时全都正常）。
//!
//! ## 两道闸门
//!
//! 1. **拖动时**夹（[`crate::ui::apply_drag`]）：想拖出屏幕也只能贴边 ——
//!    与系统给普通窗口保留标题栏可见是同一个意思；
//! 2. **启动时**夹（[`crate::ui::run`]）：配置里已经躺着屏幕外的坐标（旧版本写的、
//!    显示器拔了/换了、手改坏了）时，窗口回到**最近的那块**显示器里，
//!    并把修正后的值写回配置（否则每次启动都得再夹一遍，而且谁去看那份配置都会被误导）；
//! 3. **回写时**不记（[`crate::ui::should_save_pos`]）：位置停稳后如果仍落在屏幕外
//!    （只有"运行中显示器变了"这条路还能造出来），**不写进配置** —— 记住一个屏幕外的
//!    坐标，等于把下次启动也一起赔进去。
//!
//! ## 夹的判据是"整窗可见"
//!
//! 不是"至少留一条边"：320×380 的挂件留一条边等于只剩几十像素，照样点不到也看不见；
//! 而"整窗可见"一句话能写清、也能被单测钉死。代价是用户没法把挂件**故意**停成半截 ——
//! 对这么小的 HUD 来说这不算损失，能拖回来才是刚需。
//!
//! ## 与 `config::plausible_window_pos` 的分工
//!
//! 那个判据（`|x| ≤ 32000`）挡的是 NaN / 天文数字，**挡不住"合法的屏幕外坐标"**
//! （`-484` 轻松通过）。它的注释写着"防止窗口再也找不回来"，其实做不到；真做这件事的
//! 是本模块。两者**不合并**：那边是不依赖平台的纯函数，管"这数像不像话"；
//! 这边要问系统"Our desktop 长什么样"。
//!
//! ## 风格
//!
//! 与 `cursor.rs` / `dialog.rs` / `procinfo.rs` 一致：`#[link]` 只声明用到的那两个函数，
//! **不引 `windows` crate**。取数那一层（[`monitor_rect_at`]）没有单测（要真有显示器，
//! 而无头测试里那块屏的尺寸不该决定断言）；**夹取本身是纯函数**（[`clamp_origin`]），
//! 测试全部钉在那里。

use egui::{Pos2, Rect, Vec2};

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct WinRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// `MONITORINFO`。字段顺序与大小必须与 `winuser.h` 一致；`cb_size` 按文档填**结构体
/// 自身的大小**（系统据此判断调用方用的是哪个版本的结构）。
#[repr(C)]
struct MonitorInfo {
    cb_size: u32,
    rc_monitor: WinRect,
    rc_work: WinRect,
    dw_flags: u32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn MonitorFromPoint(pt: Point, flags: u32) -> *mut core::ffi::c_void;
    fn GetMonitorInfoW(monitor: *mut core::ffi::c_void, info: *mut MonitorInfo) -> i32;
}

/// `MONITOR_DEFAULTTONEAREST`：给一个**不在任何显示器上**的点，也返回离它最近的那块。
/// 正是我们要的语义 —— 夹取的目标就是"离它想去的地方最近的那块屏幕"。
const MONITOR_DEFAULTTONEAREST: u32 = 2;

/// 离 `p` 最近的显示器矩形（屏幕坐标；本机 `pixels_per_point = 1.0`，物理像素与点同值）。
///
/// 用 `rcMonitor`（**整块屏幕**）而不是 `rcWork`（扣掉任务栏）：夹取只在"要出界"时才生效，
/// 而"把挂件停在任务栏那一条上"是用户可能有意为之的事，不该被我们悄悄推回去。
///
/// 失败（极少见）时返回 `None` ⇒ 调用方**什么都不夹**，而不是拿别的数顶上 ——
/// 与 [`crate::cursor::screen_pos`] 同一条纪律：拿错数会把窗口推着跑，不动只是少夹一次。
pub fn monitor_rect_at(p: Pos2) -> Option<Rect> {
    // NaN / 无穷经 `as i32` 饱和成极值，这里不额外判：那种点本来就夹不动
    // （见 `clamp_origin` 开头的有限性闸门），走哪块屏都无所谓。
    let pt = Point {
        x: p.x.round() as i32,
        y: p.y.round() as i32,
    };
    // SAFETY: 无指针参数、无所有权；返回的是系统持有的显示器句柄，只用于紧接着的查询。
    let monitor = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }
    let mut info = MonitorInfo {
        cb_size: std::mem::size_of::<MonitorInfo>() as u32,
        rc_monitor: WinRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        rc_work: WinRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        dw_flags: 0,
    };
    // SAFETY: `info` 是本函数栈上的合法结构体，`cb_size` 已按文档填好；
    // 失败时返回 0，此时我们不读它的内容。
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info) };
    if ok == 0 {
        return None;
    }
    let r = info.rc_monitor;
    Some(Rect::from_min_max(
        Pos2::new(r.left as f32, r.top as f32),
        Pos2::new(r.right as f32, r.bottom as f32),
    ))
}

/// 把窗口原点夹进 `bounds`：**整窗可见**；`bounds` 比窗口还小时（显示器小于窗口）贴它的左上角。
///
/// 纯函数 ⇒ 能被单测钉住（真实显示器尺寸不由我们摆布）。多显示器下 `bounds` 仍是单块屏幕的
/// 矩形：窗口该去哪块屏由调用方决定（[`clamp_with`] 拿**窗口中心**去问）。
///
/// 有限性闸门放在最前面：任何一路是 NaN/无穷就**原样返回**。夹取的价值是"别把窗口弄丢"，
/// 数都算不出来的时候不动比乱动安全（同 `cursor` 那条"拿不到就什么都不做"）。
pub fn clamp_origin(pos: Pos2, size: Vec2, bounds: Rect) -> Pos2 {
    let finite = pos.x.is_finite()
        && pos.y.is_finite()
        && size.x.is_finite()
        && size.y.is_finite()
        && bounds.min.x.is_finite()
        && bounds.min.y.is_finite()
        && bounds.max.x.is_finite()
        && bounds.max.y.is_finite();
    if !finite {
        return pos;
    }
    // 逐轴独立夹。上限用 `.max(下界)` 兜住"窗口比屏幕还大"这种情况 ——
    // 否则 `f32::clamp` 会因为 min > max 而 panic。
    let max_x = (bounds.max.x - size.x).max(bounds.min.x);
    let max_y = (bounds.max.y - size.y).max(bounds.min.y);
    Pos2::new(
        pos.x.clamp(bounds.min.x, max_x),
        pos.y.clamp(bounds.min.y, max_y),
    )
}

/// 夹进**离它想去的地方最近的那块**屏幕。`monitor_at` 是取数口（生产传
/// [`monitor_rect_at`]，测试传夹具），拿不到就不夹。
///
/// 问的是**窗口中心**落在哪块屏，不是原点：原点在角上时可能贴着另一块屏的边，
/// 而"这块窗口主要在哪块屏上"由中心说了算。
pub fn clamp_with(pos: Pos2, size: Vec2, monitor_at: &dyn Fn(Pos2) -> Option<Rect>) -> Pos2 {
    match monitor_at(pos + size / 2.0) {
        Some(bounds) => clamp_origin(pos, size, bounds),
        None => pos,
    }
}

/// [`clamp_with`] 的生产版本：[`monitor_rect_at`] 真去问系统。
pub fn clamp_onto_nearest_monitor(pos: Pos2, size: Vec2) -> Pos2 {
    clamp_with(pos, size, &monitor_rect_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机 2026-09-30 那块屏：单屏 2560×1440，原点 (0,0)。
    fn screen() -> Rect {
        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(2560.0, 1440.0))
    }

    /// 320×380 是挂件的默认尺寸（`ui::DEFAULT_INNER_W/H`，这里写死一份是为了让本模块的
    /// 断言不跟着界面常量一起动）。
    fn widget() -> Vec2 {
        Vec2::new(320.0, 380.0)
    }

    /// **这次事故的那个数**（2026-09-30 真机）：`[-484, 603]` 整窗落在左边界外。
    #[test]
    fn the_position_that_lost_the_widget_gets_pulled_back_into_view() {
        assert_eq!(
            clamp_origin(Pos2::new(-484.0, 603.0), widget(), screen()),
            Pos2::new(0.0, 603.0),
            "整窗在左边界外 ⇒ 贴左缘；y 没出界就不许动它"
        );
    }

    /// 屏内的位置**一个像素都不许动** —— 夹取不能变成"每次启都挪一下窗口"。
    #[test]
    fn a_position_already_inside_is_left_exactly_alone() {
        for p in [
            Pos2::new(0.0, 0.0),
            Pos2::new(2200.0, 603.0),
            Pos2::new(2560.0 - 320.0, 1440.0 - 380.0), // 正好贴右下角
            Pos2::new(1234.5, 987.5),
        ] {
            assert_eq!(clamp_origin(p, widget(), screen()), p);
        }
    }

    /// 四条边都夹：右边/下边出界时不能用"贴左缘"那套（那是上一条的判据要反过来）。
    #[test]
    fn every_edge_pulls_the_window_back() {
        let s = screen();
        let w = widget();
        let off_right = Pos2::new(2500.0, 603.0); // 右缘 2820 > 2560
        assert_eq!(clamp_origin(off_right, w, s), Pos2::new(2240.0, 603.0));
        let off_bottom = Pos2::new(100.0, 1400.0); // 下缘 1780 > 1440
        assert_eq!(clamp_origin(off_bottom, w, s), Pos2::new(100.0, 1060.0));
        // 右上角出界：两个轴各夹各的
        assert_eq!(
            clamp_origin(Pos2::new(3000.0, -50.0), w, s),
            Pos2::new(2240.0, 0.0)
        );
    }

    /// 显示器比窗口还小（外接屏竖过来、远程会话被缩过……）：贴左上角，**不许 panic**。
    #[test]
    fn a_screen_smaller_than_the_window_pins_to_its_origin_instead_of_panicking() {
        let tiny = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(200.0, 200.0));
        assert_eq!(
            clamp_origin(Pos2::new(-50.0, -50.0), widget(), tiny),
            Pos2::new(0.0, 0.0)
        );
        assert_eq!(
            clamp_origin(Pos2::new(900.0, 900.0), widget(), tiny),
            Pos2::new(0.0, 0.0)
        );
    }

    /// 左/上为负的虚拟桌面（第二块屏在主屏左边）是**合法布局**：夹进那块屏不能让窗口跳回主屏。
    #[test]
    fn a_monitor_left_of_the_origin_is_a_legal_place_to_be() {
        let left_screen = Rect::from_min_max(Pos2::new(-1920.0, 0.0), Pos2::new(0.0, 1080.0));
        assert_eq!(
            clamp_origin(Pos2::new(-1900.0, 300.0), widget(), left_screen),
            Pos2::new(-1900.0, 300.0)
        );
        assert_eq!(
            clamp_origin(Pos2::new(-3000.0, 300.0), widget(), left_screen),
            Pos2::new(-1920.0, 300.0)
        );
    }

    /// 数算不出来时**原样返回**（不 panic、不把 NaN 变成一个看起来合法的坐标）。
    #[test]
    fn junk_numbers_move_nothing() {
        let s = screen();
        let w = widget();
        assert!(clamp_origin(Pos2::new(f32::NAN, 100.0), w, s).x.is_nan());
        assert_eq!(
            clamp_origin(Pos2::new(1.0e9, 100.0), Vec2::new(f32::NAN, 380.0), s),
            Pos2::new(1.0e9, 100.0),
            "尺寸算不出来时也一样：原样返回，不许把它变成一个看起来合法的坐标"
        );
    }

    /// 取数口拿不到（`None`）时**不夹**，而不是回落到"默认位置"。
    #[test]
    fn no_monitor_information_means_no_clamping() {
        let p = Pos2::new(-484.0, 603.0);
        assert_eq!(clamp_with(p, widget(), &|_| None), p);
        // 取数口收到的必须是**窗口中心**（原点 + 半个窗口）
        let seen = std::cell::Cell::new(Pos2::ZERO);
        let _ = clamp_with(Pos2::new(100.0, 200.0), widget(), &|q| {
            seen.set(q);
            None
        });
        assert_eq!(seen.get(), Pos2::new(260.0, 390.0));
    }
}
