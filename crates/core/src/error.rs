use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("未找到百度网盘安装目录")]
    InstallNotFound,

    #[error("未解析到目标用户 SID")]
    UserSidNotFound,

    #[error("存在多个候选用户，需用户选择: {0:?}")]
    AmbiguousUserSid(Vec<String>),

    #[error("目标被禁区保护拦截: {0}")]
    Forbidden(PathBuf),

    #[error("跨卷移动空间不足：需要 {need} 字节，可用 {free} 字节")]
    NoSpace { need: u64, free: u64 },

    #[error("注册表操作失败 [{hive}\\{key}]: {source}")]
    Registry {
        hive: String,
        key: String,
        #[source]
        source: windows::core::Error,
    },

    #[error("主客户端完整性校验失败，缺失: {0}")]
    ClientIntegrityBroken(String),

    #[error("数据表解析失败 [{name}]: {source}")]
    Data {
        name: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("注册表文件解析失败: {0}")]
    RegFile(String),

    #[error("IO 失败 [{path}]: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("Windows API 调用失败 [{what}]: {source}")]
    Win32 {
        what: String,
        #[source]
        source: windows::core::Error,
    },
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub fn data(name: &str, source: serde_json::Error) -> Self {
        Self::Data {
            name: name.to_owned(),
            source,
        }
    }

    pub fn win32(what: impl Into<String>, source: windows::core::Error) -> Self {
        Self::Win32 {
            what: what.into(),
            source,
        }
    }
}
