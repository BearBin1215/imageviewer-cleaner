use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 清理目标组件
///
/// 勾选状态独立保存：看图与播放器是两件独立的事，用户可以只清其中一样。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub id: ComponentId,
    /// 界面展示名，如「智能看图」
    pub label: String,
    /// 副标题：文件组件是大小，注册表组件是命中的关联项数
    pub detail: String,
    /// 默认是否勾选。用户可以在界面上改，因此组件之间互相独立。
    pub default_enabled: bool,
    /// 实际探测到的目标数量，是「该组件是否还存在」的唯一判据。
    ///
    /// 文件/目录组件等于实际存在的路径数；注册表组件没有路径，等于实际命中的
    /// 关联项数。候选清单只按数据表展开，数量固定不变，不能当作探测结果。
    pub found: usize,
}

impl Component {
    /// 该组件当前是否还存在。
    pub fn present(&self) -> bool {
        self.found > 0
    }
}

/// 从同一份变体列表定义 [`ComponentId`] 枚举与 `ALL` 清单。
///
/// 枚举与清单共享同一份 token 流，不存在「枚举里有、清单里漏」的失同步：
/// 新增变体时 `ALL` 自动包含它。
macro_rules! define_component_id {
    (
        $(#[$enum_meta:meta])*
        pub enum $name:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident
            ),* $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum $name {
            $(
                $(#[$variant_meta])*
                $variant,
            )*
        }

        impl $name {
            /// 全部组件。进入枚举就会出现在界面的可勾选清单里；
            /// 默认勾选状态由 [`Self::default_enabled`] 决定。
            ///
            /// 本清单与枚举由 [`define_component_id!`] 从同一份变体列表生成，
            /// 不需要也无法单独登记。
            pub const ALL: [Self; define_component_id!(@count $($variant)*)] =
                [$(Self::$variant),*];
        }
    };

    (@count $head:ident $($tail:ident)*) => { 1 + define_component_id!(@count $($tail)*) };
    (@count) => { 0 };
}

define_component_id! {
    /// 清理目标组件的唯一标识
    pub enum ComponentId {
        /// 智能看图。
        /// 只清空 `module\ImageViewer` 的内容并保留目录本身：
        /// 整目录移走会让客户端检测到模块缺失而无法启动，
        /// 清空内容（含入口程序）则不影响客户端启动。
        ImageViewer,
        /// 播放器启动器。
        /// 它是文件关联的处理程序入口。实测确认：移除它之后客户端仍能正常启动。
        PlayerLauncher,
        /// 旧版播放器引擎。
        ///
        /// `module\VastPlayer\vastplayer.dll` 是旧构建，运行中的宿主加载的是
        /// BrowserEngine 下的同名文件（实测基线 8.8.8.101 中二者分别为
        /// 2.2.4.10 与 2.2.4.14），它不在 `browserengine.dll` 的启动校验清单里。
        /// 实测确认：移除后客户端能正常启动并播放视频。
        ///
        /// 只点名这一个文件——同目录的 `d3dx9_43.dll` 正被插件宿主加载，必须留下。
        PlayerLegacy,
        /// 注册表关联
        RegistryAssoc,
    }
}

impl ComponentId {
    pub fn label(self) -> &'static str {
        match self {
            Self::ImageViewer => "智能看图",
            Self::PlayerLauncher => "播放器启动器",
            Self::PlayerLegacy => "播放器旧版引擎",
            Self::RegistryAssoc => "文件关联",
        }
    }

    /// 该组件清理的是目录内的条目，目录本身保留。
    pub fn moves_dir_contents(self) -> bool {
        matches!(self, Self::ImageViewer)
    }

    /// 该组件要清理的相对路径。注册表组件没有文件路径，返回空。
    pub fn relative_paths(self) -> &'static [&'static str] {
        match self {
            Self::ImageViewer => &[r"module\ImageViewer"],
            Self::PlayerLauncher => &[r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe"],
            Self::PlayerLegacy => &[r"module\VastPlayer\vastplayer.dll"],
            Self::RegistryAssoc => &[],
        }
    }

    /// 默认是否勾选。
    ///
    /// 这里刻意穷尽匹配全部变体：新增组件时若未显式登记默认勾选状态，编译会
    /// 失败，避免新组件在无人确认的情况下被默认勾选。当前全部默认勾选——它们
    /// 都是运行本工具就想清掉的东西；但清单本身是可选的，用户可以只清其中一部分。
    pub fn default_enabled(self) -> bool {
        match self {
            Self::ImageViewer | Self::PlayerLauncher | Self::PlayerLegacy | Self::RegistryAssoc => {
                true
            }
        }
    }
}

/// 探测结果
#[derive(Debug, Clone)]
pub struct Detection {
    /// 目标用户 SID（真实用户 hive，含 RID）
    pub target_user_sid: String,
    pub install_dir: PathBuf,
    pub components: Vec<Component>,
    /// 实际存在的注册表关联项，供展示与清理使用。
    /// 探测阶段已按存在性过滤，候选清单中不存在的项不在其中。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 组件清单是界面上「可勾选的项目」清单。
    ///
    /// 新增组件时必须更新本断言，确认新目标确实可以清理：它通过了禁区校验与
    /// 启动必需文件校验，默认勾选状态也在 [`ComponentId::default_enabled`] 里
    /// 显式登记。
    #[test]
    fn component_set_is_deliberate() {
        assert_eq!(
            ComponentId::ALL.len(),
            4,
            "组件数量变了：新增组件会出现在可勾选清单里，请确认新目标确实可清，再更新本断言"
        );
    }

    /// `ALL` 与枚举由 [`define_component_id!`] 从同一份变体列表生成，
    /// 不会失同步；这里核对每个变体都有展示名。
    #[test]
    fn all_lists_every_variant() {
        for id in ComponentId::ALL {
            assert!(!id.label().is_empty(), "{id:?} 缺少展示名");
        }
    }

    /// `present()` 只看 `found`。
    #[test]
    fn present_follows_found() {
        let c = Component {
            id: ComponentId::RegistryAssoc,
            label: "t".into(),
            detail: "t".into(),
            default_enabled: true,
            found: 0,
        };
        assert!(!c.present());
        let c = Component { found: 3, ..c };
        assert!(c.present());
    }
}
