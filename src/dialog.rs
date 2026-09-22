//! 唯一的"给人看"的输出通道：系统对话框。
//!
//! 挂件是常驻桌面的小窗，**绝不能带控制台黑框**（spec §9）。代价是本进程不再拥有控制台，
//! `println!` / `eprintln!` 写出去没有任何人看得见 —— 于是**所有**要让人知道的结果
//! （CLI 子命令成败、配置读不出来、挂件启动失败、崩溃、中文字体缺失）都必须走这里。
//!
//! **为什么抽成独立模块**：原先 `show_message` 住在 `main.rs`，而"取不到中文字体"这件事
//! 是在 `ui::run` 里发现的。那条路径要么把信息一路传回 `main`（扭曲两边签名），要么就
//! 沉默 —— 原先正是后者：整屏缺字方框，却什么都不说（spec §4.3 点名的失效模式）。
//!
//! **不要在这里加"顺手也写一份日志"之类的第二个通道**：项目的硬约束是零网络，用户要的
//! 是"一眼看见"，不是再多一处要去找的地方。

// `link` 只声明需要的那一个函数，不引 `windows` crate —— 本项目依赖保持精简。
// edition 2024 起 extern 块本身必须是 `unsafe extern`（rustdoc 不为 extern 块生成文档，
// 故此处用普通注释而非 `///`）。
#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBoxW(
        hwnd: *mut core::ffi::c_void,
        text: *const u16,
        caption: *const u16,
        mb: u32,
    ) -> i32;
}

const MB_ICONINFORMATION: u32 = 0x40;
const MB_ICONERROR: u32 = 0x10;

/// 模态阻塞，直到用户点「确定」为止（`MB_OK` = 0x0）。
/// `is_error` 决定图标为 ERROR 还是 INFORMATION，便于一眼分辨成败。
///
/// 说明：这条路径**无法单测**（弹框需要人点）。它的验收由人工/实机覆盖 ——
/// 尤其是 9c 要求的"加载失败必须把错误原文呈现给用户"。
pub fn show_message(title: &str, body: &str, is_error: bool) {
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let (t, b) = (wide(title), wide(body));
    let icon = if is_error { MB_ICONERROR } else { MB_ICONINFORMATION };
    unsafe { MessageBoxW(std::ptr::null_mut(), b.as_ptr(), t.as_ptr(), icon) };
}
