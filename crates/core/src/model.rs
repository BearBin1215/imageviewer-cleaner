use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 清理目标组件
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    pub id: ComponentId,
    /// 界面展示名，如「智能看图」
    pub label: String,
    /// 副标题，如「（388 MB）」
    pub detail: String,
    pub kind: ComponentKind,
    pub risk: Risk,
    pub default_enabled: bool,
    pub size_bytes: Option<u64>,
    /// 实际存在的路径。为空表示该组件当前不存在（已清理或未安装）。
    pub paths: Vec<PathBuf>,
}

impl Component {
    /// 该组件当前是否存在。
    /// 注册表关联没有具体路径，其存在与否由探测阶段单独判定，这里恒为存在。
    pub fn present(&self) -> bool {
        self.kind == ComponentKind::Registry || !self.paths.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ComponentId {
    /// 智能看图（默认勾选）。
    /// 只清空 `module\ImageViewer` 的内容并保留目录本身：
    /// 整目录移走会让客户端检测到模块缺失而无法启动，
    /// 清空内容（含入口程序）则不影响客户端启动。
    ImageViewer,
    /// 播放器启动器（默认勾选）
    PlayerLauncher,
    /// 注册表关联（默认勾选）
    RegistryAssoc,
}

impl ComponentId {
    pub fn label(self) -> &'static str {
        match self {
            Self::ImageViewer => "智能看图",
            Self::PlayerLauncher => "智能播放器",
            Self::RegistryAssoc => "文件关联",
        }
    }

    /// 该组件清理的是目录内的条目，目录本身保留。
    pub fn moves_dir_contents(self) -> bool {
        matches!(self, Self::ImageViewer)
    }

    pub fn relative_paths(self) -> &'static [&'static str] {
        match self {
            Self::ImageViewer => &[r"module\ImageViewer"],
            Self::PlayerLauncher => &[r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe"],
            Self::RegistryAssoc => &[],
        }
    }

    /// 默认的风险等级与勾选状态。
    ///
    /// 这里刻意穷尽匹配全部变体：新增组件时编译器会要求显式决定它的风险等级
    /// 与默认勾选状态，避免新组件被默认勾选而误删用户数据。
    pub fn default_risk(self) -> (Risk, bool) {
        match self {
            Self::ImageViewer | Self::PlayerLauncher | Self::RegistryAssoc => (Risk::Safe, true),
        }
    }

    pub const ALL: [Self; 3] = [Self::ImageViewer, Self::PlayerLauncher, Self::RegistryAssoc];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComponentKind {
    Directory,
    File,
    Registry,
}

/// 风险等级 —— 决定 UI 是否允许勾选
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Risk {
    /// 默认勾选
    Safe,
    /// 默认不勾选，需用户显式确认
    Caution,
    /// 永远不可操作（禁区清单来自 `data/forbidden.json`）
    Forbidden,
}

/// 探测结果
#[derive(Debug, Clone)]
pub struct Detection {
    /// 目标用户 SID（真实用户 hive，含 RID）
    pub target_user_sid: String,
    pub install_dir: PathBuf,
    pub components: Vec<Component>,
    /// 实际存在的注册表关联项（用于展示与清理）
    pub registry_items: Vec<RegItem>,
}

/// 一条待清理的注册表项
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegItem {
    pub path: RegPath,
    /// None = 删除整个键；Some(v) = 只删该值
    pub value: Option<String>,
}

/// 注册表路径 —— 用「已解析的绝对路径」表示，各后端无需再自行拼接 SID
///
/// `HKU\S-1-5-21-...-1001_Classes\Applications\BaiduNetdisk.open`
/// `HKLM\Software\Classes\BaiduNetdiskPlayerAssociations`
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RegPath {
    pub root: RegRoot,
    /// 相对 root 的子路径（不含前导反斜杠）
    pub sub: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RegRoot {
    /// HKEY_USERS
    Hku,
    /// HKEY_LOCAL_MACHINE
    Hklm,
}

impl RegPath {
    pub fn hku(sub: impl Into<String>) -> Self {
        Self {
            root: RegRoot::Hku,
            sub: sub.into(),
        }
    }

    pub fn hklm(sub: impl Into<String>) -> Self {
        Self {
            root: RegRoot::Hklm,
            sub: sub.into(),
        }
    }

    pub fn root_name(&self) -> &'static str {
        match self.root {
            RegRoot::Hku => "HKU",
            RegRoot::Hklm => "HKLM",
        }
    }

    /// 完整路径字符串，如 `HKU\S-1-5-...-1001_Classes\Applications`
    pub fn full(&self) -> String {
        if self.sub.is_empty() {
            self.root_name().to_owned()
        } else {
            format!("{}\\{}", self.root_name(), self.sub)
        }
    }

    pub fn join(&self, seg: &str) -> Self {
        let sub = if self.sub.is_empty() {
            seg.to_owned()
        } else {
            format!("{}\\{}", self.sub, seg)
        };
        Self {
            root: self.root,
            sub,
        }
    }

    /// 目标用户的 `_Classes` 路径（百度的关联键写在这里）
    pub fn user_classes(sid: &str) -> Self {
        Self::hku(format!("{sid}_Classes"))
    }

    /// 目标用户的软件配置路径
    pub fn user_software(sid: &str) -> Self {
        Self::hku(sid)
    }

    pub fn machine_classes() -> Self {
        Self::hklm(r"Software\Classes")
    }
}

// ───────────────────────── 操作记录（manifest.json） ─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub install_dir: PathBuf,
    /// 隔离区路径随 manifest 记录，还原时无需推算
    pub quarantine_dir: PathBuf,
    pub target_user_sid: String,
    pub actions: Vec<Action>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Action {
    ProcessKilled {
        name: String,
        pid: u32,
        forced: bool,
    },
    FileMoved {
        from: PathBuf,
        to: PathBuf,
        cross_volume: bool,
    },
    RegKeyDeleted {
        hive: String,
        key: String,
        exported_to: PathBuf,
    },
    RegValueDeleted {
        hive: String,
        key: String,
        value: String,
        exported_to: PathBuf,
    },
    PlaceholderCreated {
        path: PathBuf,
        kind: PlaceholderKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaceholderKind {
    /// 目录 → 同名只读文件
    FileInsteadOfDir,
    /// 文件 → 同名目录
    DirInsteadOfFile,
}
