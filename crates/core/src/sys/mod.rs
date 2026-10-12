//! 系统后端抽象
//!
//! 四个 trait 全部可注入，使进程反查、注册表、候选路径三种探测
//! 都能在集成测试里被驱动，不触碰真实系统。

pub mod fs;
#[cfg(test)]
pub mod mock;
pub mod process;
pub(crate) mod regfile;
pub mod registry;
pub mod shell;

/// 把字符串编码成以 0 结尾的 UTF-16，供 Win32 宽字符 API 使用。
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 系统能力上下文：把四个后端打包注入业务层
pub struct SysContext {
    pub registry: Box<dyn registry::RegistryBackend>,
    pub process: Box<dyn process::ProcessBackend>,
    pub fs: Box<dyn fs::FsBackend>,
    pub shell: Box<dyn shell::ShellBackend>,
}

impl SysContext {
    /// 真实 Win32 实现
    pub fn win32() -> Self {
        Self {
            registry: Box::new(registry::Win32Registry::new()),
            process: Box::new(process::Win32Process::new()),
            fs: Box::new(fs::Win32Fs::new()),
            shell: Box::new(shell::Win32Shell::new()),
        }
    }
}

impl Default for SysContext {
    fn default() -> Self {
        Self::win32()
    }
}
