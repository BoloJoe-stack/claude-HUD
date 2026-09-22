//! 进程表查询（Win32）。9b 的两半都建在它上面。
//!
//! 原先这套代码住在 `probe.rs` 里、自称"调试专用"。9b 裁定之后它有了**真实消费者**，
//! 所以独立成模块：
//!
//! 1. **hook 侧**在 `SessionStart` 抓一次宿主 pid（[`nearest_named`]）。
//! 2. **挂件侧**每轮判那个 pid 还在不在（[`alive`]）。
//!
//! ## 两条路径的成本差 4 个数量级 —— 这个分工是测出来的，不是拍的
//!
//! | 调用 | 做什么 | 实测 |
//! |---|---|---|
//! | [`snapshot`] | **枚举整个进程表** | **7.5 ms**（release；与 debug 几乎一样 ⇒ 是内核枚举的代价，不是 CPU） |
//! | [`alive`] | 对**一个** pid 两次廉价调用 | 微秒级 |
//!
//! 所以 `snapshot` **只能每会话一次**（`SessionStart`），绝不能再放进 hook 的每个事件里
//! —— hook 的启动成本本来只有约 2 ms，7.5 ms 会让它慢 4~5 倍。
//! 而挂件每轮对每个会话调一次 `alive`，代价可忽略。
//!
//! 敢这么分工，是因为**宿主 pid 对同一会话恒定**（2026-09-15 取证：一个会话 3/3、
//! 另一个 2/2，见 `docs/工作日志.md`）。抓一次就够，没有理由在热路径上反复枚举。
//!
//! ## 为什么不用依赖表里的 `sysinfo`（本批裁定：删掉该依赖）
//!
//! 1. `snapshot` 要的就是"一次快照拿到全进程表的 (pid, ppid, exe 名)"，Toolhelp 一次
//!    调用即得；而 `sysinfo::System::new_all()` 要刷新全部进程，Windows 后端为了取
//!    parent 还得对**每个**进程调一次 `NtQueryInformationProcess`。
//! 2. `alive` 要的两样（进程在不在、映像名是什么）用 `OpenProcess` +
//!    `QueryFullProcessImageNameW` 各一次就够，都是**有文档的** API。
//! 3. `main.rs` / `dialog.rs` 已经在用 `#[link(name = "…")] unsafe extern "system"`
//!    直调 Win32，注释写明"不引 `windows` crate —— 本项目依赖保持精简"。此处沿用同一
//!    模式：不新增依赖、也不引入新风格。

/// 进程表里的一行。`name` 是 exe **文件名**（Toolhelp 的 `szExeFile`），不是全路径。
pub struct ProcEntry {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
}

// ---- 父进程链（hook 侧用）----------------------------------------------------

/// 从进程表里走出 `start` 的**父进程链**（不含自身），由近到远。
/// 每项是 `(pid, 进程名)`；父进程已退出时名字是 `None`（pid 还在，进程表里查不到）。
///
/// 纯函数：喂合成表即可单测，不必真的 fork 进程。
fn ancestors(table: &[ProcEntry], start: u32) -> Vec<(u32, Option<String>)> {
    const MAX_DEPTH: usize = 32;
    let mut out = Vec::new();
    let mut seen = vec![start];
    let mut cur = start;

    while out.len() < MAX_DEPTH {
        let Some(entry) = table.iter().find(|e| e.pid == cur) else {
            break;
        };
        let parent = entry.ppid;
        // ppid 0 = 没有父进程（System Idle）。seen 防环：pid 会被复用，快照也可能
        // 自带环状数据，而死循环会让 hook 挂住 —— hook 绝不能挂住。
        if parent == 0 || seen.contains(&parent) {
            break;
        }
        seen.push(parent);
        out.push((
            parent,
            table.iter().find(|e| e.pid == parent).map(|p| p.name.clone()),
        ));
        cur = parent;
    }
    out
}

/// 父进程链的可读形式，每项形如 `cmd.exe (1234)`；父进程已退出时写 `<已退出> (1234)`。
pub fn ancestor_chain(table: &[ProcEntry], start: u32) -> Vec<String> {
    ancestors(table, start)
        .into_iter()
        .map(|(pid, name)| match name {
            Some(n) => format!("{n} ({pid})"),
            None => format!("<已退出> ({pid})"),
        })
        .collect()
}

/// 链上**最近的**那个名字匹配 `name` 的进程 pid（不区分大小写）。
///
/// 9b 关心的正是它：`claude_pid` 要记的就是"宿主是谁"。直接算出来而不是让调用方去解析
/// `"claude.exe (3528)"` 这种字符串 —— 那种解析一旦格式微调就静默失准。
///
/// **跳过名字查不到的那一跳**（父进程已退出）：不能把一个已死的 pid 当成宿主。
pub fn nearest_named(table: &[ProcEntry], start: u32, name: &str) -> Option<u32> {
    ancestors(table, start)
        .into_iter()
        .find(|(_, n)| n.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(name)))
        .map(|(pid, _)| pid)
}

/// 一次问出两件事：**本会话的宿主是谁**、**宿主之上是否还压着另一个宿主**
/// （返回 `(宿主, 上一层宿主)`；后者为 `None` = 没有，即本会话是用户直接开的）。
///
/// 第二项回答的是"这个会话是不是被**另一个会话**拉起来的"。真机取证（2026-09-18）——
/// 一个会话里用 Bash 跑 `claude -p …` 做 token 实验，那个子进程的链是
///
/// ```text
/// claude.exe(30828) ← python ← py ← bash ← bash ← bash ← claude.exe(29072) ← powershell ← Code.exe
/// ```
///
/// 子进程**自己也是一个会话**、也写状态文件，于是挂件把实验的两个 arm 当成用户开的
/// 会话各列一行：**用户开 4 个，界面显示 6 个**（用户 2026-09-18 报的就是这个）。
///
/// 判据刻意只用**父进程链的形状**，不解析命令行（`-p` / `--output-format` 那些旗标
/// 会随版本变，而链的形状是 `CreateProcess` 的必然结果）：普通会话的链是
/// `claude.exe ← powershell ← Code.exe ← explorer`，**上下只有它自己一个 claude**。
///
/// 宿主找不到时返回 `None`（**不猜**），调用方保留原值 —— 与 [`nearest_named`] 同一口径。
pub fn host_and_parent(table: &[ProcEntry], start: u32, name: &str) -> Option<(u32, Option<u32>)> {
    let host = nearest_named(table, start, name)?;
    // 从宿主**再往上一跳**：`nearest_named` 不收 `start` 自己，所以这里找到的必然是
    // 第二个 claude.exe，绝不会把宿主本人当成"上一层"。
    Some((host, nearest_named(table, host, name)))
}

/// Claude Code 宿主的 exe 名。真机取证（2026-09-15）看到的 hop 1 就是它。
pub const HOST_EXE: &str = "claude.exe";

// ---- 存活检测（挂件侧用）-----------------------------------------------------

const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const ERROR_ACCESS_DENIED: u32 = 5;

/// 映像路径的**文件名**是否就是 `exe_name`。
///
/// **不能只判相等 —— 真机上会全军覆没。** 实测（2026-09-15）Claude Code 的映像路径是
///
/// ```text
/// C:\Users\…\npm\node_modules\@anthropic-ai\claude-code\bin\claude.exe.old.1789424350591
/// ```
///
/// 它在**升级时把自己的可执行文件改名了**（运行中的 exe 不能被覆盖，只能先改名、再放新的
/// 进去）。而 Toolhelp 的 `szExeFile` 报的仍是**原名** `claude.exe` —— 同一个进程，两条
/// 路子给的答案不一样。
///
/// 按"取 basename 再判相等"实现时，**本机 3 个真在跑的 `claude.exe` 全部被判成"已退出"**；
/// 线上表现是挂件里的会话一个个莫名消失，且不报错。这是**反向对照测试**抓出来的
/// （`alive_recognizes_a_real_running_host_if_one_is_present`），不是靠读代码想出来的。
///
/// 规则因此是"相等，**或以 `exe_name.` 开头**"：`claude.exe` 与 `claude.exe.old.123` 都算，
/// 但 `claude.exefoo`、`notclaude.exe` 不算。
///
/// 逐字节比较（而不是切片成 `&str`）是为了免掉"切片落在多字节字符中间"的 panic 面 ——
/// 这里处理的是别人给的文件名。
fn image_name_matches(file: &str, exe_name: &str) -> bool {
    let (f, e) = (file.as_bytes(), exe_name.as_bytes());
    if f.len() < e.len() || !f[..e.len()].eq_ignore_ascii_case(e) {
        return false;
    }
    f.len() == e.len() || f[e.len()] == b'.'
}

/// `pid` 是否仍存活，**且映像名仍是 `exe_name`**（不区分大小写）。
///
/// 两个条件缺一不可：只看 pid 存活会被 **pid 复用**骗到 —— Windows 会回收 pid，而挂件
/// 把这个号记了很久，一个陌生进程占了同一个号就会被当成"宿主还活着"。带上映像名校验后，
/// 复用者必须**同时也是同名的可执行文件**才会误判。
///
/// ## 判不准时一律判"还活着"
///
/// 取不到映像名（权限不足、调用失败）时我们**不知道**，此时返回 `true`。理由：
/// 误判成"已退出"的后果是——先标错、两轮后**把这个会话从界面上抹掉**；而这是个帮人
/// 看"谁还在跑"的工具，**最糟的错法就是让一个活着的会话消失**。宁可漏报僵尸，不可误杀活人。
pub fn alive(pid: u32, exe_name: &str) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            // 打不开有两种原因，必须区分：
            //   进程**不存在** → 确实死了
            //   进程存在但**不让开**（更高完整性级别、系统进程）→ 它活着
            return GetLastError() == ERROR_ACCESS_DENIED;
        }
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        let _ = CloseHandle(h);
        if ok == 0 {
            return true; // 不知道 → 当作活着，见上
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let file = path.rsplit(['\\', '/']).next().unwrap_or(&path);
        image_name_matches(file, exe_name)
    }
}

// ---- Win32 ------------------------------------------------------------------

/// `PROCESSENTRY32W`（winbase.h）。`#[repr(C)]` 的填充规则与 C 一致，但字段**顺序与
/// 宽度必须逐字对上** —— 错一个，拿到的 pid 与名字就是垃圾数据。故此处用 C 里的
/// 原始类型：`ULONG_PTR` → `usize`（x64 上 8 字节），`WCHAR[MAX_PATH]` → `[u16; 260]`。
#[repr(C)]
struct ProcessEntry32W {
    dw_size: u32,
    cnt_usage: u32,
    th32_process_id: u32,
    th32_default_heap_id: usize, // ULONG_PTR
    th32_module_id: u32,
    cnt_threads: u32,
    th32_parent_process_id: u32,
    pc_pri_class_base: i32,
    dw_flags: u32,
    sz_exe_file: [u16; 260], // MAX_PATH
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GetCurrentProcessId() -> u32;
    fn GetLastError() -> u32;
    fn CreateToolhelp32Snapshot(dw_flags: u32, th32_process_id: u32) -> *mut core::ffi::c_void;
    fn Process32FirstW(snapshot: *mut core::ffi::c_void, entry: *mut ProcessEntry32W) -> i32;
    fn Process32NextW(snapshot: *mut core::ffi::c_void, entry: *mut ProcessEntry32W) -> i32;
    fn CloseHandle(object: *mut core::ffi::c_void) -> i32;
    fn OpenProcess(
        dw_desired_access: u32,
        b_inherit_handle: i32,
        dw_process_id: u32,
    ) -> *mut core::ffi::c_void;
    fn QueryFullProcessImageNameW(
        h_process: *mut core::ffi::c_void,
        dw_flags: u32,
        lp_exe_name: *mut u16,
        lpdw_size: *mut u32,
    ) -> i32;
}

const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
const INVALID_HANDLE_VALUE: isize = -1;

/// 一次快照取回整个进程表。失败返回人话（调用方要把"测不出来"换成可读证据，所以这里
/// 既不能 panic 也不能静默返回空表 —— 空表会被误读成"没有父进程"）。
///
/// ⚠️ **7.5 ms**。只许在每会话一次的地方调（见模块顶部那张表）。
pub fn snapshot() -> Result<Vec<ProcEntry>, String> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap as isize == INVALID_HANDLE_VALUE {
            return Err(format!(
                "CreateToolhelp32Snapshot 失败：{}",
                std::io::Error::last_os_error()
            ));
        }
        // dwSize 必须先填：系统靠它校验结构版本
        let mut entry: ProcessEntry32W = std::mem::zeroed();
        entry.dw_size = std::mem::size_of::<ProcessEntry32W>() as u32;

        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                out.push(ProcEntry {
                    pid: entry.th32_process_id,
                    ppid: entry.th32_parent_process_id,
                    name: utf16_to_string(&entry.sz_exe_file),
                });
                // 走到表尾时返回 0（ERROR_NO_MORE_FILES），不是错误
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    Ok(out)
}

/// NUL 结尾的 UTF-16 定长数组 → String（exe 名可能是非 ASCII，如中文用户名下的路径）。
fn utf16_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(pid: u32, ppid: u32, name: &str) -> ProcEntry {
        ProcEntry { pid, ppid, name: name.into() }
    }

    /// 真机拓扑的合成版：hook → claude.exe → powershell.exe → Code.exe
    #[test]
    fn chain_walks_up_to_the_root() {
        let table = vec![
            e(30, 20, "claude-hud.exe"),
            e(20, 10, "claude.exe"),
            e(10, 4, "powershell.exe"),
            e(4, 0, "explorer.exe"),
        ];
        assert_eq!(
            ancestor_chain(&table, 30),
            vec!["claude.exe (20)", "powershell.exe (10)", "explorer.exe (4)"]
        );
    }

    #[test]
    fn chain_stops_at_ppid_zero_and_at_exited_parents() {
        // ppid 0 = 没有父进程，链到此为止
        let table = vec![e(5, 0, "top.exe")];
        assert!(ancestor_chain(&table, 5).is_empty());

        // 父进程已退出：pid 还在、名字查不到 —— 必须显示成"已退出"而不是丢掉这一跳，
        // 否则会误读成"链只有这么长"
        let table = vec![e(5, 9, "hook.exe")];
        assert_eq!(ancestor_chain(&table, 5), vec!["<已退出> (9)"]);

        // 自己都不在表里（快照异常）
        assert!(ancestor_chain(&[], 5).is_empty());
    }

    #[test]
    fn chain_cannot_loop_forever() {
        // pid 复用/坏数据会造出环：hook 绝不能因此挂住
        let table = vec![e(1, 2, "a.exe"), e(2, 1, "b.exe")];
        assert_eq!(ancestor_chain(&table, 1), vec!["b.exe (2)"]);

        // 自环
        let table = vec![e(7, 7, "self.exe")];
        assert!(ancestor_chain(&table, 7).is_empty());
    }

    #[test]
    fn nearest_named_finds_the_host_not_just_the_immediate_parent() {
        // 9b 关心的就是这一条：`claude_pid` 该记谁。真机上观察到的是
        // `hook → claude.exe → powershell.exe → Code.exe`，即宿主**就是**直接父进程；
        // 但若中间插了一层 shell，也应当能跳过它找到宿主。
        let table = vec![
            e(30, 20, "claude-hud.exe"),
            e(20, 10, "claude.exe"),
            e(10, 4, "powershell.exe"),
            e(4, 0, "Code.exe"),
        ];
        assert_eq!(nearest_named(&table, 30, "claude.exe"), Some(20));
        assert_eq!(nearest_named(&table, 30, "powershell.exe"), Some(10));
        assert_eq!(nearest_named(&table, 30, HOST_EXE.to_uppercase().as_str()), Some(20), "必须大小写不敏感");
        assert_eq!(nearest_named(&table, 30, "explorer.exe"), None);
    }

    #[test]
    fn host_and_parent_spot_a_session_another_session_started() {
        // 两条链都是 2026-09-18 从真机 `Win32_Process` 逐跳抄下来的（只改了 pid 数字）。
        //
        // 子会话：实验会话 29072 里用 Bash 跑 `claude -p`，链上**有两个** claude.exe。
        let child = vec![
            e(60, 50, "claude-hud.exe"), // hook 自己
            e(50, 40, "claude.exe"),     // 子会话的宿主
            e(40, 30, "python.exe"),
            e(30, 20, "bash.exe"),
            e(20, 10, "claude.exe"), // 父会话的宿主 —— 这一跳就是"子会话"的证据
            e(10, 4, "powershell.exe"),
            e(4, 0, "Code.exe"),
        ];
        assert_eq!(host_and_parent(&child, 60, HOST_EXE), Some((50, Some(20))));

        // 用户自己开的会话：链上**只有它自己一个** claude.exe（VS Code 终端起的也一样）。
        let top = vec![
            e(60, 50, "claude-hud.exe"),
            e(50, 40, "claude.exe"),
            e(40, 30, "powershell.exe"),
            e(30, 4, "Code.exe"),
            e(4, 0, "explorer.exe"),
        ];
        assert_eq!(host_and_parent(&top, 60, HOST_EXE), Some((50, None)));
    }

    #[test]
    fn host_and_parent_does_not_guess_when_the_host_is_missing() {
        // 宿主都找不到 ⇒ `None`（调用方保留原值）。绝不能返回 `Some((?, Some(..)))`
        // 让挂件凭空把会话判成子会话 —— 那会把一个真会话从界面上抹掉。
        let table = vec![e(60, 50, "claude-hud.exe"), e(50, 40, "nope.exe")];
        assert_eq!(host_and_parent(&table, 60, HOST_EXE), None);
    }

    #[test]
    fn nearest_named_skips_a_hop_whose_process_already_exited() {
        // 名字查不到的那一跳不能算命中 —— 否则会把一个已死的 pid 当成宿主记进 `claude_pid`。
        let table = vec![e(30, 20, "claude-hud.exe"), e(20, 9, "whatever.exe")];
        assert_eq!(nearest_named(&table, 30, "claude.exe"), None);
    }

    #[test]
    fn snapshot_returns_a_plausible_process_table() {
        // 真机调用（Win32）：能拿到本进程，且父进程 pid 非 0 —— 否则快照结构体布局
        // 就是错的（这一条是 `ProcessEntry32W` 字段顺序的自检）。
        let table = snapshot().expect("本机必须能取到进程表");
        assert!(table.len() > 5, "进程表不可能只有几项：{}", table.len());
        let me = unsafe { GetCurrentProcessId() };
        let mine = table.iter().find(|p| p.pid == me).expect("快照里必须有本进程");
        assert!(mine.name.to_lowercase().contains("claude"), "本进程名：{}", mine.name);
        let chain = ancestor_chain(&table, me);
        assert!(!chain.is_empty(), "测试进程必然有父进程（cargo/测试宿主）");
    }

    #[test]
    fn image_name_matches_accepts_the_renamed_self_upgrade_form() {
        // 真机实测的形态：升级时 Claude Code 把自己改名成 `claude.exe.old.<时间戳>`。
        assert!(image_name_matches("claude.exe", "claude.exe"));
        assert!(image_name_matches("claude.exe.old.1789424350591", "claude.exe"));
        assert!(image_name_matches("CLAUDE.EXE", "claude.exe"), "大小写不敏感");
        assert!(image_name_matches("Claude.exe.OLD.1", "claude.exe"));

        // 不能宽到把别的可执行文件也收进来 —— 这是 pid 复用防护的全部价值所在。
        assert!(!image_name_matches("claude.exefoo", "claude.exe"));
        assert!(!image_name_matches("notclaude.exe", "claude.exe"));
        assert!(!image_name_matches("claude", "claude.exe"));
        assert!(!image_name_matches("claude-exe", "claude.exe"));
        assert!(!image_name_matches("", "claude.exe"));
    }

    #[test]
    fn alive_recognizes_a_real_running_host_if_one_is_present() {
        // **反向对照：`alive` 的正向路径必须拿真进程验一次。**
        //
        // 验的是这条链的假设：`QueryFullProcessImageNameW` → 取 basename → 与 `"claude.exe"`
        // 比较。万一它返回的形态和我以为的不一样（没有扩展名、路径分隔符不同、大小写规则
        // 不同……），**线上每个会话都会被判成"已退出"**，而且不 panic、不报错 ——
        // 症状是"挂件上的会话莫名其妙一个个消失"。
        //
        // 这与字体那一轮同构：**判据成立与否，必须拿真实存在的样本钉住，不能靠推断。**
        let table = snapshot().expect("本机必须能取到进程表");
        let hosts: Vec<u32> = table
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case(HOST_EXE))
            .map(|p| p.pid)
            .collect();
        if hosts.is_empty() {
            eprintln!("跳过：本机当前没有 {HOST_EXE} 在跑");
            return;
        }
        for pid in &hosts {
            assert!(
                alive(*pid, HOST_EXE),
                "pid {pid} 是真在跑的 {HOST_EXE}，`alive` 必须判为存活 —— \
                 否则线上每个会话都会被标成已退出"
            );
        }
        eprintln!("已核验 {} 个真在跑的 {} 全部判为存活", hosts.len(), HOST_EXE);
    }

    #[test]
    fn alive_is_true_for_self_and_false_for_a_pid_that_cannot_exist() {
        // 自证：本进程一定活着，而且映像名就是本测试二进制的名字。
        let me = unsafe { GetCurrentProcessId() };
        let table = snapshot().expect("本机必须能取到进程表");
        let mine = table.iter().find(|p| p.pid == me).expect("快照里必须有本进程").name.clone();

        assert!(alive(me, &mine), "自己必须被判为活着（这是 alive 的自检）");
        // 映像名不匹配 → 不是"宿主"。pid 复用防护靠的就是这一条。
        assert!(!alive(me, "definitely-not-this.exe"), "映像名不符时不能判为存活");

        // 一个几乎不可能存在的 pid：Windows 的 pid 是 4 的倍数、且远小于 u32::MAX。
        assert!(!alive(u32::MAX - 3, "claude.exe"), "不存在的 pid 必须判为已退出");
    }
}
