use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// spec §4.3：必须手配。transcript 里没有任何窗口字段可推断。
    pub context_limit: u64,
    /// 上下文占比达到此值即转琥珀色。**有真实读者**：`ui.rs` 里给"上下文 N%"着色的那处。
    ///
    /// （更正，2026-09-14：本字段上方原先写着"界面侧已无读者、保留待 M4" —— 那是界面改造
    /// **第一轮**移除占比行时写的。**第二轮已把占比行加回来**，`ui.rs` 重新读了这两个阈值，
    /// 但当时没同步这段注释。以代码为准：现在有读者。）
    pub warn_threshold: u8,
    /// 同 `warn_threshold`：**有真实读者**（`ui.rs` 的占比着色）。
    pub danger_threshold: u8,
    pub poll_interval_ms: u64,
    pub idle_after_sec: u64,
    pub always_on_top: bool,
    pub window_pos: [f32; 2],
}

// 合并前删除的两个字段（均有文档、无读者 = 不兑现的旋钮）：
//   - `model_overrides`（spec §8 原写"模型 ID → 上限，精确匹配优先"）：全仓无一处读它。
//     与 `always_on_top` 被硬编码忽略是同一类缺陷 —— 用户设了它、没有任何效果、
//     也没有任何提示。将来真要按模型分上限（例如切换模型后窗口不同）时再加，
//     并**同时**加上读它的代码与测试。
//   - `stale_after_sec`（原写"0 = 完成后一直留在列表里"）：同样无读者。僵尸会话
//     回收的真正方案由计划 9b 的实测结论决定，届时再加**正确的**旋钮。
// 两者都是配置项，删掉对已有 config.json 无影响：本结构是 `#[serde(default)]` 且
// serde 默认忽略未知字段，旧文件里的这两个键仍能正常加载（多余键被丢弃）。
//
// 界面改造（2026-09-14）删除的第三个字段：
//   - `opacity: f32`（默认 0.92）：它唯一的含义是面板**填充色的 alpha**。界面改成
//     **不透明白底**之后这个含义不再存在 —— 白底必须完全不透明，否则桌面从白底里
//     透出来、黑描边与黑字一起糊掉。留着它就又是一个"用户设了、没有任何效果"的
//     旋钮（与上面两个同属一类，用户裁定一并删除）。
//     对已有 config.json 无影响：同一条 serde 前提 —— 旧文件里的 `"opacity": 0.92`
//     会被丢弃，而不是让整份配置解析失败（否则 `load_from` 会静默回落默认值，
//     用户手配的 `context_limit` 无声变回 1_000_000，正是 spec §4.3 点名的失效）。
//     这条前提由下面的 `legacy_opacity_key_is_ignored` 钉住。
impl Default for Config {
    fn default() -> Self {
        Self {
            context_limit: 1_000_000,
            warn_threshold: 50,
            danger_threshold: 75,
            poll_interval_ms: 1000,
            idle_after_sec: 300,
            always_on_top: true,
            window_pos: [100.0, 100.0],
        }
    }
}

/// 读配置。`Err` = **文件存在但读不出来**（IO 错 / JSON 坏），不是"文件不存在"。
fn read_config(path: &Path) -> Result<Config, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        // 文件不存在 = 首次运行，用默认值是正确的，**不是**故障
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("无法读取：{e}")),
    };
    serde_json::from_str(&text).map_err(|e| format!("JSON 解析失败：{e}"))
}

/// 文件不存在或 JSON 损坏时一律退回默认值——挂件不能因为配置坏了就不启动。
///
/// **静默**：`--hook` 路径用它，绝不能弹出任何东西（hook 必须无人值守地退出 0）。
/// GUI 路径要的是"让人看见"，用 [`report_broken`] 而不是这个。
pub fn load_from(path: &Path) -> Config {
    read_config(path).unwrap_or_default()
}

/// 配置损坏时的处置：**存档**一份原文件，并返回一段给用户看的说明。
/// 文件不存在（首次运行）或一切正常时返回 `None`。
///
/// 为什么必须报出来（spec §4.3 点名的本项目最危险失效模式）：`unwrap_or_default()`
/// 会把"用户手配的 `context_limit`"静默换回 `1_000_000` —— 百分比看起来完全合理、
/// 不会报错，只会让人在错误的时机做决策。
///
/// 为什么要存档：M4 起挂件会回写 `window_pos`，届时这份"坏了但可人工修复"的内容
/// 会被静默覆盖掉。
pub fn report_broken(path: &Path) -> Option<String> {
    let reason = read_config(path).err()?;
    let backup = PathBuf::from(format!("{}.broken", path.display()));
    let note = if std::fs::copy(path, &backup).is_ok() {
        format!("原文件已另存为：\n{}", backup.display())
    } else {
        "原文件无法另存（复制也失败），请勿让挂件覆盖它".to_string()
    };
    Some(format!(
        "{} 存在但读不出来，已改用默认值：\n\n原因：{}\n\n{}",
        path.display(),
        reason,
        note
    ))
}

/// 临时文件名对每次调用唯一：把 pid 与线程 id 拼进目标文件名末尾。
/// 固定名（`config.json.tmp`）会让两个并发 `save_to` 共用同一个 tmp —— B 可能在 A 的
/// `write` 与 `rename` 之间截断该文件，A 随即把截断/空的 tmp rename 到位，而 B 自己的
/// `rename` 因 tmp 已消失而 `NotFound` —— 正是原子写要消灭的失效（progress.md 第 14 条）。
/// 唯一名同时让"落在旧固定名上的遗留物"不再能阻塞保存（第 15 条）。
fn tmp_for(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{:?}.tmp",
        std::process::id(),
        std::thread::current().id()
    ));
    target.with_file_name(name)
}

/// 原子写：先写同目录同卷的具名兄弟临时文件，再 `rename` 覆盖目标。
/// 直写会在中途被打断时（进程被杀 / 磁盘满）留下截断的 config.json，
/// 而 `load_from` 对损坏 JSON 是静默全量回落默认值——用户手配的 context_limit
/// 会无提示地变回 1_000_000。改走 tmp + rename 后，目标文件要么是旧值要么是新值。
/// 这个坐标值不值得信（挂件**回写** `window_pos` 之前先过这一关）。
///
/// 存在的理由：显示器拔了/换了、或配置被手改坏，一个落在屏幕外的坐标会让窗口
/// **再也找不回来**（无边框窗口没有标题栏，拖不回来就等于消失）。
/// 判据刻意宽松（只挡明显是垃圾的值）：虚屏坐标可以是负的，多显示器下也很正常。
pub fn plausible_window_pos(p: [f32; 2]) -> bool {
    const LIMIT: f32 = 32_000.0;
    p[0].is_finite() && p[1].is_finite() && p[0].abs() <= LIMIT && p[1].abs() <= LIMIT
}

pub fn save_to(path: &Path, cfg: &Config) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cfg).unwrap();
    // 唯一名兄弟路径（config.json → config.json.<pid>.<tid>.tmp），与目标同目录同卷，
    // 保证 rename 是原子的；唯一性保证两个并发写者不会共用同一个 tmp
    let tmp = tmp_for(path);
    std::fs::write(&tmp, json)?;
    // rename 是提交点；失败时必须删掉自己的 tmp —— 唯一名不像固定名那样会被下一次写
    // 覆盖而自愈，留着就会在配置目录里越积越多
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn default_context_limit_is_one_million() {
        // spec §4.3：上限必须手配，默认 1M，绝不推断
        assert_eq!(Config::default().context_limit, 1_000_000);
    }

    #[test]
    fn default_idle_threshold_is_300s() {
        let c = Config::default();
        assert_eq!(c.idle_after_sec, 300);
    }

    #[test]
    fn report_broken_distinguishes_missing_from_unreadable() {
        // E：'文件不存在'（首次运行，用默认值是对的）与'存在但读不出来'（故障，
        // 必须让人看见）是两件事，绝不能混成一个 `unwrap_or_default()`。
        let dir = tempdir();
        let missing = dir.join("nope.json");
        assert!(report_broken(&missing).is_none(), "文件不存在不是故障，不该报警");
        assert_eq!(load_from(&missing).context_limit, 1_000_000);

        let broken = dir.join("config.json");
        fs::write(&broken, br#"{"context_limit": 128000, "poll_interval_ms":"#).unwrap();
        let msg = report_broken(&broken).expect("存在但读不出来必须报出来");
        assert!(msg.contains("config.json"), "说明里必须带路径，否则用户不知道修哪个：{msg}");
        // 原文件被存档，且**未被改动**（M4 起挂件会回写 window_pos）
        let backup = PathBuf::from(format!("{}.broken", broken.display()));
        assert!(backup.exists(), "损坏的配置必须先另存一份");
        assert_eq!(
            fs::read_to_string(&broken).unwrap(),
            r#"{"context_limit": 128000, "poll_interval_ms":"#
        );
        assert_eq!(fs::read_to_string(&backup).unwrap(), fs::read_to_string(&broken).unwrap());
        // 正常文件不报
        fs::write(&broken, r#"{"context_limit": 128000}"#).unwrap();
        assert!(report_broken(&broken).is_none());
        assert_eq!(load_from(&broken).context_limit, 128_000);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = tempdir();
        let p = dir.join("nope.json");
        assert_eq!(load_from(&p).context_limit, 1_000_000);
    }

    #[test]
    fn partial_file_keeps_defaults_for_absent_fields() {
        let dir = tempdir();
        let p = dir.join("config.json");
        fs::write(&p, r#"{"context_limit": 128000}"#).unwrap();
        let c = load_from(&p);
        assert_eq!(c.context_limit, 128_000);   // 被覆盖
        assert_eq!(c.idle_after_sec, 300);      // 保持默认
        assert_eq!(c.warn_threshold, 50);
    }

    #[test]
    fn unknown_keys_in_an_old_config_are_ignored_not_fatal() {
        // 删掉 `model_overrides` / `stale_after_sec` / `opacity` 三个字段的**安全前提**：
        // serde 默认忽略未知字段（本结构没有 `deny_unknown_fields`），所以用户既有的
        // config.json 里留着这些键仍能正常加载。
        //
        // 这条前提此前只是"显然"，没有任何测试钉住 —— 而它一旦不成立，删字段就会让
        // **整份**配置解析失败：`load_from` 静默回落默认值，用户手配的 context_limit
        // 无声变回 1_000_000（正是 spec §4.3 点名、E 修掉的那个失效模式）。
        let dir = tempdir();
        let p = dir.join("config.json");
        fs::write(
            &p,
            r#"{"context_limit":128000,"model_overrides":{"deepseek-flash":200000},"stale_after_sec":43200}"#,
        )
        .unwrap();
        assert_eq!(
            load_from(&p).context_limit,
            128_000,
            "旧文件里多出的键不得让整份配置解析失败"
        );
        assert_eq!(load_from(&p).idle_after_sec, 300, "其余字段照常取默认值");
        assert!(
            report_broken(&p).is_none(),
            "多出未知键不是'读不出来'，不该报警"
        );
    }

    #[test]
    fn legacy_opacity_key_is_ignored() {
        // 本机（以及任何跑过旧版本的机器）的 config.json 里都还留着 `"opacity": 0.92`
        // —— 界面改造把它从结构体里删掉了。删除本身必须**对老文件无害**：多出来的这个
        // 键要被忽略，而不是让整份配置解析失败（那会静默回落默认值，用户手配的
        // context_limit 无声变回 1_000_000，正是 spec §4.3 点名的失效模式）。
        //
        // 与 `unknown_keys_in_an_old_config_are_ignored_not_fatal` 的差别：那条用的是
        // "本来就没在结构体里出现过"的键，这条用的是**刚刚被删掉的**键 —— 后者才是
        // 这次改动真正会碰到的真实文件。
        let dir = tempdir();
        let p = dir.join("config.json");
        fs::write(&p, r#"{"context_limit":128000,"opacity":0.92,"always_on_top":false}"#).unwrap();

        let c = load_from(&p);
        assert_eq!(c.context_limit, 128_000, "被删掉的 opacity 不得拖垮整份配置");
        assert!(!c.always_on_top, "同一个文件里的其它键必须照常生效");
        assert!(
            report_broken(&p).is_none(),
            "留着 opacity 不是'读不出来'，不该弹框报警"
        );
        // 再存回去：序列化结果里不得再有 opacity（字段确实删干净了，不是只是不读它）
        save_to(&p, &c).unwrap();
        assert!(
            !fs::read_to_string(&p).unwrap().contains("opacity"),
            "存回去的 JSON 里不得再有 opacity：{}",
            fs::read_to_string(&p).unwrap()
        );
        assert_eq!(load_from(&p).context_limit, 128_000, "存回去之后仍要能读回同一个值");
    }

    #[test]
    fn round_trip_preserves_values() {
        let dir = tempdir();
        let p = dir.join("config.json");
        let mut c = Config::default();
        c.poll_interval_ms = 250;
        c.window_pos = [42.0, 99.0];
        save_to(&p, &c).unwrap();
        let back = load_from(&p);
        assert_eq!(back.poll_interval_ms, 250);
        assert_eq!(back.window_pos, [42.0, 99.0]);
    }

    #[test]
    fn save_to_is_atomic_and_leaves_no_tmp_file_behind() {
        let dir = tempdir();
        let p = dir.join("config.json");
        // 预置一个"旧版本／崩溃残留"的固定名陈旧临时文件。改成唯一名后本实现不再触碰
        // 这个路径（这正是第 14 条裁定的目的：落在旧固定名上的遗留物不得再干扰保存），
        // 所以它允许留在盘上 —— 但也**仅此一个**：save_to 自己不得新增任何 tmp，
        // 且绝不许写入它（否则就成了"拿遗留物当提交物"）。
        let stale_tmp = dir.join("config.json.tmp");
        fs::write(&stale_tmp, r#"{"context_limit": 1}"#).unwrap();

        let mut c = Config::default();
        c.context_limit = 128_000;
        c.poll_interval_ms = 250;
        c.window_pos = [42.0, 99.0];
        save_to(&p, &c).unwrap();

        // (a) 往返读回的值正确（原子写的另一半：目标文件必须是完整 JSON）
        let back = load_from(&p);
        assert_eq!(back.context_limit, 128_000);
        assert_eq!(back.poll_interval_ms, 250);
        assert_eq!(back.window_pos, [42.0, 99.0]);
        // 目标文件不得等同于遗留 tmp 的内容（防止"复用了别人的半成品"）
        assert_ne!(back.context_limit, 1);

        // (b) 除预置的陈旧文件外不残留任何 .tmp —— 唯一名实现不复用固定名 tmp，
        //     残留只可能来自预置，绝不可能来自 save_to 自己
        let mut leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        leftovers.sort();
        assert_eq!(
            leftovers,
            vec!["config.json.tmp".to_string()],
            "save_to 不应新增 tmp：{leftovers:?}"
        );
        // 且预置文件的字节未被改动
        assert_eq!(fs::read_to_string(&stale_tmp).unwrap(), r#"{"context_limit": 1}"#);
    }

    #[test]
    fn failed_save_leaves_the_existing_config_byte_identical() {
        // 账本 #17：'失败写入不破坏既有配置' 此前没有**提交级**测试。rename 是唯一的
        // 提交点，它失败时目标文件必须一个字节都没动 —— 否则用户手配的 context_limit
        // 就被半截内容毁掉了，而 `load_from` 对损坏 JSON 是静默回落默认值，
        // 用户不会看到任何错误（spec §4.3 点名的那类失效）。
        // 手法与 `hooks_install::failed_rename_cleans_up_its_own_tmp` 一致：
        // 目标是只读文件时，Windows 上 rename 覆盖必然失败。
        let dir = tempdir();
        let p = dir.join("config.json");
        let mut good = Config::default();
        good.context_limit = 128_000;
        good.poll_interval_ms = 250;
        save_to(&p, &good).unwrap();
        let before = fs::read_to_string(&p).unwrap();

        let mut perms = fs::metadata(&p).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&p, perms).unwrap();

        let mut other = Config::default();
        other.context_limit = 42;
        let res = save_to(&p, &other);

        // 先复位只读位再断言，避免 tempdir 下一轮的 remove_dir_all 被挡住
        let mut open = fs::metadata(&p).unwrap().permissions();
        open.set_readonly(false);
        fs::set_permissions(&p, open).unwrap();

        assert!(res.is_err(), "目标是只读文件时 rename 必须失败");
        // ① 目标文件逐字节未变 —— 这是本用例的核心断言
        assert_eq!(
            fs::read_to_string(&p).unwrap(),
            before,
            "失败路径不得改动既有配置（一个字节都不行）"
        );
        // ② 读回来仍是用户手配的值，而不是新值、也不是默认值
        let back = load_from(&p);
        assert_eq!(back.context_limit, 128_000, "既有配置必须仍然可读且原样");
        assert_eq!(back.poll_interval_ms, 250);
        assert_ne!(back.context_limit, 42, "失败的那次写入不得部分生效");
        // ③ 不残留自己的 tmp（唯一名 tmp 不会被下一次写覆盖而自愈）
        // 局限：只读目标让 `write` 阶段就无法通过，所以本用例区分不了"先截断再失败"
        // 的实现；它钉的是同一条契约的另一半 —— 提交点失败时目标必须完好。
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "rename 失败后不应残留 tmp：{leftovers:?}");
    }

    use crate::testtmp::TempDir;

    fn tempdir() -> TempDir {
        // 无 tag：本文件的用例共用一个名字前缀，唯一性由 `TempDir` 的进程内序号保证
        // （原先靠 `{:?}` 打印线程 id 区分，而并行测试的线程分配不可依赖）。
        TempDir::new("test", "")
    }
}
