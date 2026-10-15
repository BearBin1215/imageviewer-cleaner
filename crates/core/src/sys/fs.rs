//! 文件系统后端 —— trait 抽象 + Win32 实现
//!
//! 隔离区**必须与安装目录同卷**，否则 `MoveFileEx` 退化成复制加删除：
//! 非原子、大文件耗时数秒、需要目标卷有等量空余空间，中途被锁还会留下半完成状态。
//! 本层把「同卷原子改名」与「跨卷复制、校验、删源」两条路径都封装起来。

use std::path::{Path, PathBuf};

use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_REPARSE_POINT, GetVolumePathNameW};
use windows::core::PCWSTR;

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOutcome {
    pub to: PathBuf,
    /// 是否为跨卷移动（非原子，走了 copy+校验+删源）
    pub cross_volume: bool,
    pub bytes: u64,
}

pub trait FsBackend: Send + Sync {
    fn exists(&self, p: &Path) -> bool;
    fn is_dir(&self, p: &Path) -> bool;
    /// 目录存在且没有任何条目（探测「清空目录内容」型组件是否已清理用）
    fn is_empty_dir(&self, p: &Path) -> bool;
    /// 文件大小或目录递归大小
    fn size_of(&self, p: &Path) -> Result<u64>;
    /// 路径所在卷的可用空间
    fn free_space(&self, p: &Path) -> Result<u64>;
    fn same_volume(&self, a: &Path, b: &Path) -> bool;
    /// 设置隐藏属性（Windows）
    fn set_hidden(&self, p: &Path) -> Result<()>;
    /// 路径本身是否是重解析点（目录联接 / 符号链接 / 其他 reparse point）
    fn is_reparse_point(&self, p: &Path) -> bool;
    /// 解析重解析点之后，`target` 是否仍位于 `root` 之内
    ///
    /// `root` 自身允许是重解析点（安装目录本身可能就是一个联接），
    /// 但 `root` 之下任何一段都不允许——那意味着目标实际位于别处。
    /// 纯词法前缀比较挡不住这种情况，移动与占位前必须先过这一关。
    fn resolves_within(&self, root: &Path, target: &Path) -> bool;
    /// 把目录权限收紧为「仅 SYSTEM 与 Administrators 完全控制」并断开继承
    fn restrict_to_admins(&self, p: &Path) -> Result<()>;
    /// 移动（同卷为原子改名；跨卷为 copy+校验+删源）
    fn move_path(&self, from: &Path, to: &Path) -> Result<MoveOutcome>;
    fn ensure_dir(&self, p: &Path) -> Result<()>;
    /// 占位：在期望是目录的位置放一个同名只读**文件**
    fn create_placeholder_file(&self, p: &Path) -> Result<()>;
    /// 占位：在期望是文件的位置放一个同名**目录**
    fn create_placeholder_dir(&self, p: &Path) -> Result<()>;
    /// 删除文件或目录（还原时移除占位物用）
    fn remove_path(&self, p: &Path) -> Result<()>;
}

/// 递归遍历的深度上限。
///
/// 联接（junction）在 Windows 上不需要任何特权即可创建，自指或互为祖先的联接
/// 会让无深度限制的递归耗尽栈空间（发布构建是 `panic = "abort"`，进程会直接终止）。
/// 目录层级远超这个数字的安装目录不存在，因此撞到上限只可能是环。
const MAX_DEPTH: usize = 64;

/// 路径是否是重解析点（不跟随最后一个组件）
fn is_reparse(p: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(p)
        .map(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0)
        .unwrap_or(false)
}

/// 按 SDDL 字符串替换路径的 DACL
fn set_dacl(p: &Path, sddl: &str) -> Result<()> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1, SE_FILE_OBJECT,
        SetNamedSecurityInfoW,
    };
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    let sddl = crate::sys::wide(sddl);
    let mut psd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut psd,
            None,
        )
    }
    .map_err(|e| Error::win32("ConvertStringSecurityDescriptorToSecurityDescriptorW", e))?;

    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut present = windows::core::BOOL(0);
    let mut defaulted = windows::core::BOOL(0);
    let got = unsafe { GetSecurityDescriptorDacl(psd, &mut present, &mut dacl, &mut defaulted) };
    if let Err(e) = got {
        unsafe { LocalFree(Some(HLOCAL(psd.0))) };
        return Err(Error::win32("GetSecurityDescriptorDacl", e));
    }
    if dacl.is_null() {
        unsafe { LocalFree(Some(HLOCAL(psd.0))) };
        return Err(Error::io(p, std::io::Error::other("安全描述符里没有 DACL")));
    }

    let name = crate::sys::wide(&p.to_string_lossy());
    let rc = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl as *const ACL),
            None,
        )
    };
    unsafe { LocalFree(Some(HLOCAL(psd.0))) };
    if rc.is_err() {
        return Err(Error::win32(
            format!("SetNamedSecurityInfoW({})", p.display()),
            rc.to_hresult().into(),
        ));
    }
    Ok(())
}

/// 读回路径的 DACL 并转成 SDDL 文本（测试用）
#[cfg(test)]
fn read_dacl(p: &Path) -> Result<String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR};

    let name = crate::sys::wide(&p.to_string_lossy());
    let mut psd = PSECURITY_DESCRIPTOR::default();
    let rc = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            &mut psd,
        )
    };
    if rc.is_err() {
        return Err(Error::win32(
            format!("GetNamedSecurityInfoW({})", p.display()),
            rc.to_hresult().into(),
        ));
    }
    let mut text = windows::core::PWSTR::null();
    let r = unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            psd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut text,
            None,
        )
    };
    let out = match r {
        Ok(()) => unsafe { text.to_string() }.unwrap_or_default(),
        Err(e) => {
            unsafe { LocalFree(Some(HLOCAL(psd.0))) };
            return Err(Error::win32(
                "ConvertSecurityDescriptorToStringSecurityDescriptorW",
                e,
            ));
        }
    };
    unsafe {
        LocalFree(Some(HLOCAL(text.0 as *mut _)));
        LocalFree(Some(HLOCAL(psd.0)));
    }
    Ok(out)
}

fn volume_of(p: &Path) -> Option<String> {
    let w = crate::sys::wide(&p.to_string_lossy());
    let mut buf = [0u16; 260];
    if unsafe { GetVolumePathNameW(PCWSTR(w.as_ptr()), &mut buf) }.is_err() {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]).to_uppercase())
}

fn dir_size(p: &Path) -> std::io::Result<u64> {
    dir_size_at(p, 0)
}

fn dir_size_at(p: &Path, depth: usize) -> std::io::Result<u64> {
    if depth >= MAX_DEPTH {
        return Ok(0);
    }
    if p.is_file() {
        return Ok(p.metadata()?.len());
    }
    let mut total = 0u64;
    for e in std::fs::read_dir(p)? {
        let e = e?;
        // 重解析点直接跳过，不计入大小：跟随会让统计走到安装目录之外，
        // 自指联接还会让递归无法结束
        if is_reparse(&e.path()) {
            continue;
        }
        let t = e.file_type()?;
        if t.is_dir() {
            total += dir_size_at(&e.path(), depth + 1)?;
        } else {
            total += e.metadata()?.len();
        }
    }
    Ok(total)
}

// ───────────────────────────────── Win32 实现 ─────────────────────────────────

pub struct Win32Fs;

impl Win32Fs {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Win32Fs {
    fn default() -> Self {
        Self::new()
    }
}

impl FsBackend for Win32Fs {
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }

    fn is_dir(&self, p: &Path) -> bool {
        p.is_dir()
    }

    fn is_empty_dir(&self, p: &Path) -> bool {
        std::fs::read_dir(p)
            .map(|mut it| it.next().is_none())
            .unwrap_or(false)
    }

    fn size_of(&self, p: &Path) -> Result<u64> {
        dir_size(p).map_err(|e| Error::io(p, e))
    }

    fn free_space(&self, p: &Path) -> Result<u64> {
        // 向上找到已存在的祖先目录再查询
        let mut probe = p.to_path_buf();
        while !probe.exists() {
            match probe.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => probe = parent.to_path_buf(),
                _ => {
                    return Err(Error::io(
                        p,
                        std::io::Error::other("无法定位已存在的祖先目录"),
                    ));
                }
            }
        }
        let w = crate::sys::wide(&probe.to_string_lossy());
        let mut free = 0u64;
        let mut total = 0u64;
        let ok = unsafe {
            windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
                PCWSTR(w.as_ptr()),
                Some(&mut free),
                Some(&mut total),
                None,
            )
        };
        if ok.is_err() {
            return Err(Error::io(
                p,
                std::io::Error::other("GetDiskFreeSpaceExW 失败"),
            ));
        }
        Ok(free)
    }

    fn same_volume(&self, a: &Path, b: &Path) -> bool {
        match (volume_of(a), volume_of(b)) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        }
    }

    fn set_hidden(&self, p: &Path) -> Result<()> {
        use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, SetFileAttributesW};
        let w = crate::sys::wide(&p.to_string_lossy());
        unsafe { SetFileAttributesW(PCWSTR(w.as_ptr()), FILE_ATTRIBUTE_HIDDEN) }
            .map_err(|e| Error::win32(format!("SetFileAttributesW({})", p.display()), e))
    }

    fn is_reparse_point(&self, p: &Path) -> bool {
        is_reparse(p)
    }

    fn resolves_within(&self, root: &Path, target: &Path) -> bool {
        let Ok(rel) = target.strip_prefix(root) else {
            return false;
        };
        if rel.as_os_str().is_empty() {
            return true;
        }
        let mut cur = root.to_path_buf();
        for seg in rel.components() {
            cur.push(seg.as_os_str());
            if is_reparse(&cur) {
                return false;
            }
        }
        true
    }

    /// 把目录权限收紧为「仅 SYSTEM 与 Administrators 完全控制」，并断开继承。
    ///
    /// `D:P` 表示受保护的 DACL：不继承父目录的任何 ACE，因此无论目录由谁创建，
    /// 收紧之后都只剩 `SY`（SYSTEM）与 `BA`（Administrators）两条。为什么要收紧
    /// 见 [`crate::backup::Quarantine::prepare`]。
    fn restrict_to_admins(&self, p: &Path) -> Result<()> {
        set_dacl(p, "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")
    }

    fn move_path(&self, from: &Path, to: &Path) -> Result<MoveOutcome> {
        if !from.exists() {
            return Err(Error::io(from, std::io::Error::other("源不存在")));
        }
        let bytes = self.size_of(from)?;

        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        // 同卷：原子改名
        if self.same_volume(from, to) {
            std::fs::rename(from, to).map_err(|e| Error::io(from, e))?;
            return Ok(MoveOutcome {
                to: to.to_path_buf(),
                cross_volume: false,
                bytes,
            });
        }

        // 跨卷移动：依次检查空间、复制、校验大小、删除源文件，任一步失败则整体回滚
        let free = self.free_space(to)?;
        if free < bytes {
            return Err(Error::NoSpace { need: bytes, free });
        }

        copy_recursive(from, to).map_err(|e| Error::io(from, e))?;
        let copied = dir_size(to).unwrap_or(0);
        if copied != bytes {
            let _ = std::fs::remove_dir_all(to);
            let _ = std::fs::remove_file(to);
            return Err(Error::io(
                from,
                std::io::Error::other(format!(
                    "跨卷复制校验失败：源 {bytes} 字节，副本 {copied} 字节"
                )),
            ));
        }

        // 校验通过才删源
        if from.is_dir() {
            std::fs::remove_dir_all(from).map_err(|e| Error::io(from, e))?;
        } else {
            std::fs::remove_file(from).map_err(|e| Error::io(from, e))?;
        }

        Ok(MoveOutcome {
            to: to.to_path_buf(),
            cross_volume: true,
            bytes,
        })
    }

    fn ensure_dir(&self, p: &Path) -> Result<()> {
        std::fs::create_dir_all(p).map_err(|e| Error::io(p, e))
    }

    fn create_placeholder_file(&self, p: &Path) -> Result<()> {
        if p.exists() {
            return Ok(());
        }
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        std::fs::write(p, b"").map_err(|e| Error::io(p, e))?;
        // 设为只读，增加更新器直接覆盖的难度
        let mut perm = std::fs::metadata(p)
            .map_err(|e| Error::io(p, e))?
            .permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(p, perm).map_err(|e| Error::io(p, e))?;
        Ok(())
    }

    fn create_placeholder_dir(&self, p: &Path) -> Result<()> {
        if p.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(p).map_err(|e| Error::io(p, e))
    }

    fn remove_path(&self, p: &Path) -> Result<()> {
        if !p.exists() {
            return Ok(());
        }
        if p.is_dir() {
            // 占位目录可能是空的；非空则递归删除（还原时只对占位物调用）
            std::fs::remove_dir_all(p).map_err(|e| Error::io(p, e))
        } else {
            // Windows 下清除只读属性的标准做法；clippy 的 Unix 提示在此不适用
            #[allow(clippy::permissions_set_readonly_false)]
            fn clear_readonly(p: &Path) -> std::io::Result<()> {
                let mut perm = std::fs::metadata(p)?.permissions();
                if perm.readonly() {
                    perm.set_readonly(false);
                    std::fs::set_permissions(p, perm)?;
                }
                Ok(())
            }
            clear_readonly(p).map_err(|e| Error::io(p, e))?;
            std::fs::remove_file(p).map_err(|e| Error::io(p, e))
        }
    }
}

/// 递归复制，返回复制的条目数
///
/// 与 `dir_size` 同样跳过重解析点并限深：跨卷移动的目标目录里若有联接，
/// 跟着复制会把安装目录之外的内容也搬进隔离区。
fn copy_recursive(from: &Path, to: &Path) -> std::io::Result<usize> {
    copy_recursive_at(from, to, 0)
}

fn copy_recursive_at(from: &Path, to: &Path, depth: usize) -> std::io::Result<usize> {
    if depth >= MAX_DEPTH {
        return Ok(0);
    }
    if from.is_file() {
        std::fs::copy(from, to)?;
        return Ok(1);
    }
    std::fs::create_dir_all(to)?;
    let mut n = 1;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        if is_reparse(&e.path()) {
            continue;
        }
        let dst = to.join(e.file_name());
        n += copy_recursive_at(&e.path(), &dst, depth + 1)?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn link_dir(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn unlink_dir(link: &Path) {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "rmdir"])
            .arg(link)
            .status();
    }

    /// 收紧之后目录上只剩 SYSTEM 与 Administrators，且继承被断开。
    ///
    /// 这条断言是「隔离区不再被普通用户改写」的唯一强制点：SDDL 写错一个字母
    /// 就会静默退化成什么都不做，而功能测试察觉不到。
    #[test]
    fn restrict_to_admins_strips_inherited_aces() {
        let dir = temp("bc_fs_dacl");
        let fs = Win32Fs::new();

        let before = read_dacl(&dir).unwrap();
        assert!(before.starts_with("D:"), "应能读到 DACL：{before}");

        fs.restrict_to_admins(&dir).unwrap();
        let after = read_dacl(&dir).unwrap();
        assert!(
            after.starts_with("D:P"),
            "必须是受保护的 DACL（断开继承）：{after}"
        );
        assert!(after.contains(";;;SY)"), "应保留 SYSTEM：{after}");
        assert!(after.contains(";;;BA)"), "应保留 Administrators：{after}");
        assert!(
            !after.contains(";;;AU)"),
            "不得保留 Authenticated Users：{after}"
        );
        assert!(
            !after.contains(";;;BU)"),
            "不得保留 BUILTIN\\Users：{after}"
        );
        assert!(!after.contains(";;;WD)"), "不得保留 Everyone：{after}");

        // 恢复权限以便清理：测试进程通常没有提权，收紧后自己都删不掉
        set_dacl(&dir, "D:(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;BU)").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_within_rejects_paths_through_junctions() {
        let base = temp("bc_fs_junction");
        let root = base.join("install");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("module")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"x").unwrap();

        let fs = Win32Fs::new();
        assert!(fs.resolves_within(&root, &root.join(r"module\ImageViewer")));
        assert!(!fs.resolves_within(&root, &outside.join("secret.txt")));

        let link = root.join("module").join("ImageViewer");
        if !link_dir(&link, &outside) {
            println!("跳过：无法创建 junction");
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        assert!(fs.is_reparse_point(&link), "联接本身应被识别");
        assert!(!fs.resolves_within(&root, &link), "联接本身不得通过");
        assert!(
            !fs.resolves_within(&root, &link.join("secret.txt")),
            "经过联接的路径不得通过"
        );
        assert!(
            !fs.resolves_within(&root, &root.join(r"module\ImageViewer\secret.txt")),
            "词法上在安装目录内、实际位于别处的路径不得通过"
        );

        unlink_dir(&link);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 自指联接不得让递归耗尽栈空间：发布构建是 `panic = "abort"`，
    /// 耗尽栈空间会让进程直接终止
    #[test]
    fn dir_size_survives_junction_loops() {
        let base = temp("bc_fs_loop");
        let module = base.join("module");
        let inner = module.join("ImageViewer");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("a.dll"), b"1234").unwrap();

        let loop_link = inner.join("loop");
        if !link_dir(&loop_link, &inner) {
            println!("跳过：无法创建 junction");
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        let fs = Win32Fs::new();
        let n = fs.size_of(&inner).expect("统计应正常返回，不爆栈");
        assert_eq!(n, 4, "只应统计真实文件，联接不计入：{n}");

        unlink_dir(&loop_link);
        let _ = std::fs::remove_dir_all(&base);
    }
}
