//! 光标的**屏幕坐标**（Win32 直调）。
//!
//! ## 为什么需要一个独立模块
//!
//! 挂件的缩放是"自己算"的（见 `ui::apply_resize`）：每帧读光标位置，推出新的窗口几何，
//! 再发 `OuterPosition` + `InnerSize`。**光标位置必须与窗口位置无关** —— 否则会形成
//! 正反馈：窗口被自己推着跑，每帧越推越远。
//!
//! egui 手里的指针是"窗口内坐标"，窗口一移动它就过期；把它与"当前窗口原点"相加，
//! 相当于把**窗口自己的位移**又算了一遍成"鼠标移动"。实测（2026-09-17）：按住左缘不动
//! 6 秒，窗口从 5942 涨到 6582pt、原点跑到 x = −4020，最后整个程序卡死。
//!
//! `GetCursorPos` 给的是**屏幕坐标**，窗口怎么动都不影响它 ⇒ 那条回路根本不存在。
//!
//! ## 风格
//!
//! 与 `dialog.rs` / `procinfo.rs` 一致：`#[link]` 只声明需要的那一个函数，
//! **不引 `windows` crate** —— 本项目依赖保持精简（edition 2024 起 extern 块必须是
//! `unsafe extern`；rustdoc 不为 extern 块生成文档，故用普通注释）。
//!
//! ⚠️ **本模块的函数没有单测**（要真有鼠标与屏幕）。它是"取数"那一层；
//! 用这个数的几何计算（`ui::apply_resize`）是纯函数，那边有测试钉着 ——
//! 包括"光标不动 ⇒ 几何不动"这条**正是本次事故的不变量**。

#[repr(C)]
struct Point {
    x: i32,
    y: i32,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetCursorPos(p: *mut Point) -> i32;
}

/// 光标的屏幕坐标（egui 点；本机 `pixels_per_point = 1.0`，物理像素与点同值）。
///
/// 失败（极少见，例如会话被切换）时返回 `None` —— 调用方**什么都不做**，
/// 而不是拿别的数顶上：缩放期间少发一帧命令无害，拿错数会推着窗口跑。
pub fn screen_pos() -> Option<egui::Vec2> {
    let mut p = Point { x: 0, y: 0 };
    // SAFETY: `p` 是本函数栈上的合法结构体；`GetCursorPos` 只写这一个结构，
    // 且按文档在失败时返回 0（此时我们不读它的内容）。
    let ok = unsafe { GetCursorPos(&mut p) };
    if ok == 0 {
        None
    } else {
        Some(egui::vec2(p.x as f32, p.y as f32))
    }
}
