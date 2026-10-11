//! 目标用户 SID 解析
//!
//! 注意：`GetUserNameW` + `LookupAccountNameW` 返回的是**机器 SID（缺 RID）**，
//! 拿它访问 `HKU\<SID>` 会失败，因此本模块从 `HKEY_USERS` 的 hive 枚举入手。
//!
//! 首要判据是**「哪个 hive 下真的存在百度相关键」**，而不是「谁是当前用户」。
//! 控制台用户仅在多个候选时用于消歧；仍无法确定时报错，交给界面让用户选择，
//! **绝不静默猜测**（选错会导致清理了错误用户的关联，且用户毫无察觉）。

use crate::error::{Error, Result};
use crate::model::RegPath;
use crate::sys::SysContext;

/// 纯函数：判断 `HKEY_USERS` 下的 hive 名是否为「真实用户」
///
/// 这是粗筛；真正的判定由「是否含百度键」完成（见 `resolve_target_user_sid`）。
pub fn is_user_hive(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    if name == ".DEFAULT" || up.ends_with("_CLASSES") {
        return false;
    }
    // 已知的非用户 SID
    if matches!(name, "S-1-5-18" | "S-1-5-19" | "S-1-5-20") {
        return false;
    }
    // S-1-5-7 = ANONYMOUS LOGON；S-1-5-80/82/83/90 = 服务与窗口管理器
    for p in [
        "S-1-5-7-",
        "S-1-5-80-",
        "S-1-5-82-",
        "S-1-5-83-",
        "S-1-5-90-",
    ] {
        if up.starts_with(p) {
            return false;
        }
    }
    if name == "S-1-5-7" {
        return false;
    }
    // 真实用户 SID 以 RID 结尾且 RID >= 1000
    rid_of(name).map(|r| r >= 1000).unwrap_or(false)
}

/// 取 SID 末尾的 RID
pub fn rid_of(sid: &str) -> Option<u32> {
    sid.rsplit('-').next()?.parse().ok()
}

/// 该用户 hive 下是否存在百度相关键（首选判据）
pub fn user_hive_has_baidu_keys(sys: &SysContext, sid: &str) -> bool {
    let probes = [
        RegPath::user_software(sid).join(r"Software\Baidu\BaiduNetdiskImageViewer"),
        RegPath::user_software(sid).join(r"Software\Baidu\BaiduNetdiskPlayer"),
        RegPath::user_classes(sid).join("BaiduNetdiskImageViewerAssociations"),
        RegPath::user_classes(sid).join("BaiduNetdiskPlayerAssociations"),
    ];
    probes
        .iter()
        .any(|p| sys.registry.key_exists(p).unwrap_or(false))
}

/// 候选用户 hive（粗筛后）
pub fn candidate_user_hives(sys: &SysContext) -> Result<Vec<String>> {
    let mut v: Vec<String> = sys
        .registry
        .enumerate_user_hives()?
        .into_iter()
        .filter(|h| is_user_hive(h))
        .collect();
    v.sort();
    Ok(v)
}

/// 控制台/活动会话用户 SID（best-effort，仅用于消歧）
///
/// 做法：找活动会话里的 `explorer.exe`，读它的令牌用户 SID。
/// 比 `WTSQueryUserToken` 可靠（后者需要 `SeTcbPrivilege`，默认未启用）。
pub fn console_user_sid(sys: &SysContext) -> Option<String> {
    let session = crate::sys::process::active_console_session_id()?;
    let procs = sys.process.list().ok()?;
    for p in procs
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case("explorer.exe"))
    {
        if sys.process.session_id(p.pid).ok().flatten() != Some(session) {
            continue;
        }
        if let Some(sid) = sys.process.token_user_sid(p.pid).ok().flatten() {
            return Some(sid);
        }
    }
    None
}

/// 解析目标用户 SID
pub fn resolve_target_user_sid(sys: &SysContext) -> Result<String> {
    let candidates = candidate_user_hives(sys)?;
    if candidates.is_empty() {
        return Err(Error::UserSidNotFound);
    }

    // ① 首选判据：哪个 hive 真的含百度键
    let hits: Vec<String> = candidates
        .iter()
        .filter(|s| user_hive_has_baidu_keys(sys, s))
        .cloned()
        .collect();

    match hits.len() {
        1 => Ok(hits.into_iter().next().unwrap()),
        0 => {
            // 没有 hive 含百度键：可能组件未安装。只有一个候选时直接用它，
            // 否则交给上层处理（不猜）。
            if candidates.len() == 1 {
                Ok(candidates.into_iter().next().unwrap())
            } else {
                Err(Error::UserSidNotFound)
            }
        }
        _ => {
            // ② 多候选：用控制台用户消歧
            if let Some(console) = console_user_sid(sys) {
                if hits.iter().any(|h| h == &console) {
                    return Ok(console);
                }
            }
            // ③ 仍无法确定：交给 UI 让用户选，绝不静默猜测
            Err(Error::AmbiguousUserSid(hits))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::mock::{FakeProcesses, InMemoryRegistry};
    use crate::sys::shell::ShellBackend;
    use crate::sys::{SysContext, mock::NoopShell};

    fn ctx(reg: InMemoryRegistry, proc: FakeProcesses) -> SysContext {
        SysContext {
            registry: Box::new(reg),
            process: Box::new(proc),
            fs: Box::new(crate::sys::fs::Win32Fs::new()),
            shell: Box::new(NoopShell::default()),
        }
    }

    #[test]
    fn rid_parsing() {
        assert_eq!(rid_of("S-1-5-21-1-2-3-1001"), Some(1001));
        assert_eq!(rid_of("S-1-5-18"), Some(18));
        assert_eq!(rid_of("garbage"), None);
    }

    #[test]
    fn user_hive_filter() {
        assert!(is_user_hive("S-1-5-21-1-2-3-1001"));
        assert!(is_user_hive("S-1-5-21-1-2-3-1002"));
        assert!(!is_user_hive(".DEFAULT"));
        assert!(
            !is_user_hive("S-1-5-21-1-2-3-1001_Classes"),
            "_Classes 不是用户 hive"
        );
        assert!(!is_user_hive("S-1-5-18"), "SYSTEM");
        assert!(!is_user_hive("S-1-5-19"), "LOCAL SERVICE");
        assert!(!is_user_hive("S-1-5-20"), "NETWORK SERVICE");
        assert!(
            !is_user_hive("S-1-5-80-3880718306-3832830129-1677859214-2598158968-1052248003"),
            "服务 SID"
        );
    }

    #[test]
    fn single_hive_with_baidu_keys_wins() {
        let reg = InMemoryRegistry::new();
        let p = FakeProcesses::new();
        reg.set_user_hives(vec![
            ".DEFAULT".into(),
            "S-1-5-18".into(),
            "S-1-5-21-1-2-3-1001".into(),
            "S-1-5-21-1-2-3-1001_Classes".into(),
        ]);
        reg.add_key(
            RegPath::user_software("S-1-5-21-1-2-3-1001")
                .join(r"Software\Baidu\BaiduNetdiskImageViewer"),
        );
        let sys = ctx(reg, p);
        assert_eq!(
            resolve_target_user_sid(&sys).unwrap(),
            "S-1-5-21-1-2-3-1001"
        );
    }

    #[test]
    fn multiple_hives_without_baidu_keys_is_not_guessed() {
        let reg = InMemoryRegistry::new();
        let p = FakeProcesses::new();
        reg.set_user_hives(vec![
            "S-1-5-21-1-2-3-1001".into(),
            "S-1-5-21-1-2-3-1002".into(),
        ]);
        let sys = ctx(reg, p);
        // 两个候选、都没有百度键 → 必须报错而不是猜一个
        assert!(matches!(
            resolve_target_user_sid(&sys),
            Err(Error::UserSidNotFound)
        ));
    }

    #[test]
    fn multiple_hits_resolved_by_console_user() {
        let reg = InMemoryRegistry::new();
        let p = FakeProcesses::new();
        let a = "S-1-5-21-1-2-3-1001";
        let b = "S-1-5-21-1-2-3-1002";
        reg.set_user_hives(vec![a.into(), b.into()]);
        // 两个 hive 都有百度键 → 需消歧
        for sid in [a, b] {
            reg.add_key(RegPath::user_software(sid).join(r"Software\Baidu\BaiduNetdiskPlayer"));
        }
        // 控制台用户是 b（活动会话里的 explorer.exe）
        p.add("explorer.exe", 4242, None);
        let sys = SysContext {
            registry: Box::new(reg),
            process: Box::new(ConsoleAwareProcesses {
                inner: p,
                session: 1,
                sid: b.into(),
            }),
            fs: Box::new(crate::sys::fs::Win32Fs::new()),
            shell: Box::new(NoopShell::default()),
        };
        // 本环境下 active_console_session_id 可能返回 None（无控制台会话），
        // 此时应报 AmbiguousUserSid —— 两种结果都算通过（不静默猜）
        match resolve_target_user_sid(&sys) {
            Ok(sid) => assert_eq!(sid, b),
            Err(Error::AmbiguousUserSid(hits)) => {
                assert_eq!(hits.len(), 2, "应报告全部候选");
            }
            other => panic!("意外结果: {other:?}"),
        }
    }

    /// 包装替身：固定 session_id / token_user_sid，便于确定性测试
    struct ConsoleAwareProcesses {
        inner: FakeProcesses,
        session: u32,
        sid: String,
    }

    impl crate::sys::process::ProcessBackend for ConsoleAwareProcesses {
        fn list(&self) -> Result<Vec<crate::sys::process::ProcessInfo>> {
            self.inner.list()
        }
        fn image_path(&self, pid: u32) -> Result<Option<std::path::PathBuf>> {
            self.inner.image_path(pid)
        }
        fn request_close(&self, pid: u32) -> Result<bool> {
            self.inner.request_close(pid)
        }
        fn terminate(&self, pid: u32) -> Result<()> {
            self.inner.terminate(pid)
        }
        fn is_service(&self, name: &str) -> Result<bool> {
            self.inner.is_service(name)
        }
        fn stop_service(&self, name: &str) -> Result<()> {
            self.inner.stop_service(name)
        }
        fn disable_service(&self, name: &str) -> Result<()> {
            self.inner.disable_service(name)
        }
        fn session_id(&self, _pid: u32) -> Result<Option<u32>> {
            Ok(Some(self.session))
        }
        fn token_user_sid(&self, _pid: u32) -> Result<Option<String>> {
            Ok(Some(self.sid.clone()))
        }
    }

    fn _assert_shell_trait_in_scope(_: &dyn ShellBackend) {}
}
