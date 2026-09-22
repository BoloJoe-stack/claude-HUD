//! 测试临时目录的**统一**辅助（整个模块只在 `cfg(test)` 下编译）。
//!
//! 存在理由有两个，缺一不可：
//!
//! 1. **不再各写一份。** 此前 7 个 `src/` 模块 + 1 个集成测试各写了一份 `tempdir()`，
//!    是同一段代码的第 8 份近似副本（账本第 53 条点名）。
//! 2. **回收。** 那些副本创建后**都不删除** —— 跑一次 `cargo test` 就往 `%TEMP%` 里丢
//!    几十个目录，跨任务累积到 **3090 个**（2026-09-15 实测并清理）。这里返回一个持有者，
//!    `Drop` 时递归删除；**测试 panic 时也会执行**（栈展开照常跑 Drop）。
//!
//! ⚠️ **必须把它绑到一个变量上。** `let _ = TempDir::new(..)` 会**立刻**析构、当场删目录；
//! `TempDir::new(..).join("x")` 同理（临时值在语句末尾析构，留下的 `PathBuf` 指向一个
//! 已经不存在的目录）。要"先建目录、再取一个不存在的子路径"，分成两句写。
//!
//! ⚠️ **集成测试（`tests/`）用不了这个模块**：本 crate 只有 bin target、没有 lib target，
//! 所以 `tests/hook_e2e.rs` 自带一份同类实现。**两边要一起改**，别只改这儿。

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 一个测试专用的临时目录，`Drop` 时递归删除。
///
/// 实现了 [`Deref<Target = Path>`]，所以 `d.join("x")`、把 `&d` 传给收 `&Path` 的函数
/// 都照旧能写 —— 这是为了换掉旧 `tempdir()` 时**不改任何调用点**。
pub struct TempDir(PathBuf);

impl TempDir {
    /// 建一个临时目录，名字形如 `claude-hud-<prefix>-<pid>-<seq>[-<tag>]`。
    ///
    /// **`seq` 是进程内递增的**，所以同一个 tag 调多少次都互不冲突 —— 这一点是必需的：
    /// `config.rs` 的 8 个用例共用一个无 tag 的辅助，原先靠 `{:?}` 打印线程 id 来区分，
    /// 而并行测试的线程分配是不可依赖的。有了 `seq`，名字天然唯一。
    ///
    /// 名字里保留 `tag` 是为了排查"哪个用例漏了回收"时能一眼认出来。
    pub fn new(prefix: &str, tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut name = format!("claude-hud-{prefix}-{}-{seq}", std::process::id());
        if !tag.is_empty() {
            name.push('-');
            name.push_str(tag);
        }
        let dir = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&dir).expect("建测试临时目录失败");
        Self(dir)
    }
}

impl Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // 删不掉就算了（杀软占用、别的用例的竞态）—— **绝不能 panic**：
        // 在栈展开过程中 panic 会直接 abort，把测试失败的现场毁掉。
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
