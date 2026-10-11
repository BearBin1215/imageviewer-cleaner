//! 文件系统后端 —— trait 抽象 + Win32 实现
//!
//! 隔离区**必须与安装目录同卷**，否则 `MoveFileEx` 退化成复制加删除：
//! 非原子、大文件耗时数秒、需要目标卷有等量空余空间，中途被锁还会留下半完成状态。
//! 本层把「同卷原子改名」与「跨卷复制、校验、删源」两条路径都封装起来。

use std::path::{Path, PathBuf};

use windows::Win32::Storage::FileSystem::GetVolumePathNameW;
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
    /// 文件大小或目录递归大小
    fn size_of(&self, p: &Path) -> Result<u64>;
    /// 路径所在卷的可用空间
    fn free_space(&self, p: &Path) -> Result<u64>;
    fn same_volume(&self, a: &Path, b: &Path) -> bool;
    /// 路径所在卷的根，如 `D:\`。
    /// 隔离区必须落在安装目录所在的卷上，否则移动会退化成跨卷复制。
    fn volume_root(&self, p: &Path) -> Result<PathBuf>;
    /// 设置隐藏属性（Windows）
    fn set_hidden(&self, p: &Path) -> Result<()>;
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

fn volume_of(p: &Path) -> Option<String> {
    let w = p
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    let mut buf = [0u16; 260];
    if unsafe { GetVolumePathNameW(PCWSTR(w.as_ptr()), &mut buf) }.is_err() {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]).to_uppercase())
}

fn dir_size(p: &Path) -> std::io::Result<u64> {
    if p.is_file() {
        return Ok(p.metadata()?.len());
    }
    let mut total = 0u64;
    for e in std::fs::read_dir(p)? {
        let e = e?;
        let t = e.file_type()?;
        if t.is_dir() {
            total += dir_size(&e.path())?;
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
        let w = probe
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>();
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

    fn volume_root(&self, p: &Path) -> Result<PathBuf> {
        // 向上找到已存在的祖先，再取它所在的卷
        let mut probe = p.to_path_buf();
        while !probe.exists() {
            match probe.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => probe = parent.to_path_buf(),
                _ => break,
            }
        }
        volume_of(&probe)
            .map(PathBuf::from)
            .ok_or_else(|| Error::io(p, std::io::Error::other("无法确定所在卷")))
    }

    fn set_hidden(&self, p: &Path) -> Result<()> {
        use windows::Win32::Storage::FileSystem::{
            SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN,
        };
        let w = p
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>();
        unsafe { SetFileAttributesW(PCWSTR(w.as_ptr()), FILE_ATTRIBUTE_HIDDEN) }
            .map_err(|e| Error::win32(format!("SetFileAttributesW({})", p.display()), e))
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

        // 跨卷：先查空间 → copy → 校验大小 → 删源；任一步失败整体回滚
        let free = self.free_space(to)?;
        if free < bytes {
            return Err(Error::NoSpace { need: bytes, free });
        }

        let staged = copy_recursive(from, to).map_err(|e| Error::io(from, e))?;
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
        let _ = staged;

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
fn copy_recursive(from: &Path, to: &Path) -> std::io::Result<usize> {
    if from.is_file() {
        std::fs::copy(from, to)?;
        return Ok(1);
    }
    std::fs::create_dir_all(to)?;
    let mut n = 1;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dst = to.join(e.file_name());
        n += copy_recursive(&e.path(), &dst)?;
    }
    Ok(n)
}
