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
            /// 本清单与枚举由 `define_component_id!` 从同一份变体列表生成，
            /// 不需要也无法单独登记。
            pub const ALL: [Self; define_component_id!(@count $($variant)*)] =
                [$(Self::$variant),*];
        }
    };

    (@count $head:ident $($tail:ident)*) => { 1 + define_component_id!(@count $($tail)*) };
    (@count) => { 0 };
}

/// 动态清理目标：在 `dir` 下枚举文件名以 `prefix` 开头的条目。
///
/// 用于名字里带时间戳、无法写成固定路径的目标（如更新程序留下的
/// `vastplayer.dll.old20260608T084445`）。展开只发生在运行时，因此新增的
/// 备份文件会被自动纳入，无需改数据表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixTarget {
    /// 相对安装目录的父目录
    pub dir: &'static str,
    /// 文件名前缀（不区分大小写）
    pub prefix: &'static str,
}

define_component_id! {
    /// 清理目标组件的唯一标识
    ///
    /// 划分单位是「用户想移除哪个捆绑功能」，不是文件。播放器的启动器、引擎残留、
    /// 更新备份、暂存下载包合成一项：它们要么一起失效，要么一起还有意义，拆开
    /// 只会让用户替代码做归类决定。
    pub enum ComponentId {
        /// 智能看图。
        /// 只清空 `module\ImageViewer` 的内容并保留目录本身：
        /// 整目录移走会让客户端检测到模块缺失而无法启动，
        /// 清空内容（含入口程序）则不影响客户端启动。
        ImageViewer,
        /// 智能播放器。清理目标见 [`Self::relative_paths`] 与 [`Self::prefix_targets`]，
        /// 每一项都实测确认「移除后客户端仍能正常启动并播放视频」。
        ///
        /// 播放器真正在用的引擎 `module\BrowserEngine\vastplayer.dll`，以及
        /// `videoipc.dll` / `dls.dll` / `localplayer.dll` / 整个 `resources` 子树都不在其中
        /// ——它们与主客户端共用或被启动校验核对，是禁区（理由记在 `data/forbidden.json`）。
        /// 换句话说，这项清理掉的是播放器的**入口与残留**，让功能失效，而不是移除引擎本体。
        Player,
        /// 文件关联。
        ///
        /// 与上面两个组件并列，不并入其中：这些键横跨看图与播放器，而且 `UserChoice`
        /// 受 Windows 哈希保护，删掉只能让系统重新询问，无法精确还原。
        /// 可逆性不同的事必须分开勾。
        RegistryAssoc,
    }
}

impl ComponentId {
    pub fn label(self) -> &'static str {
        match self {
            Self::ImageViewer => "智能看图",
            Self::Player => "智能播放器",
            Self::RegistryAssoc => "文件关联",
        }
    }

    /// 该组件清理的是目录内的条目，目录本身保留。
    pub fn moves_dir_contents(self) -> bool {
        matches!(self, Self::ImageViewer)
    }

    /// 该组件要清理的固定相对路径。注册表组件没有文件路径，返回空。
    ///
    /// 这里逐条记下每个目标是什么、为什么删得掉——它们合并成一个界面选项之后，
    /// 这份清单就是唯一的解释出处。
    pub fn relative_paths(self) -> &'static [&'static str] {
        match self {
            // 看图：整个目录内容，见 moves_dir_contents
            Self::ImageViewer => &[r"module\ImageViewer"],
            Self::Player => &[
                // 启动器：文件关联的处理程序入口。没有它，播放器即使代码包被重新
                // 下载也无法被调用，所以要占位。
                r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe",
                // 旧构建引擎：运行中的宿主加载的是 BrowserEngine 下的同名文件
                // （实测基线 8.8.8.101 中二者分别为 2.2.4.10 与 2.2.4.14），它不在
                // browserengine.dll 的启动校验清单里。同样要占位。
                r"module\VastPlayer\vastplayer.dll",
                // 上面那份旧引擎的 D3D9 运行库。插件宿主加载的确实是这一份（按模块
                // 完整路径确认），但实测确认移除后宿主会回落到 System32 下的同一份
                // DirectX 9 副本，客户端照常启动。
                // `module\VastPlayer` 在下发清单里的条目数为 0，该目录不会再被重建。
                r"module\VastPlayer\d3dx9_43.dll",
                // 暂存目录里被放弃的旧下载：先落盘为 `<名字>.new` + `<名字>.sig`，校验
                // 通过才搬进 resources。本机的 .new（md5 e69ea3f2…）与已生效的
                // resources\video_player.asar（md5 a1eac2d6…，与下发清单声明值一致）
                // 对不上。只碰暂存目录，不碰 resources 子树。
                // 不占位：更新程序下次会重新写入一份，而它怎么校验、何时搬走，都未实测过。
                r"module\BrowserEngine\module\asar\video_player.asar.new",
                r"module\BrowserEngine\module\asar\video_player.asar.sig",
            ],
            Self::RegistryAssoc => &[],
        }
    }

    /// 该组件的动态清理目标（目录 + 文件名前缀）。穷尽匹配，新增变体必须显式登记。
    ///
    /// 用于名字里带时间戳、无法写成固定路径的目标。
    pub fn prefix_targets(self) -> &'static [PrefixTarget] {
        match self {
            Self::Player => &[PrefixTarget {
                dir: r"module\BrowserEngine",
                // 更新程序每次替换引擎都把旧版本改名留在原地，本机已累积 9 个、共
                // 221 MB；这些名字不在下发清单里，唯一保留价值是支持更新回滚。
                // 前缀必须严格长于禁区文件 `vastplayer.dll`，否则展开会把正在被加载
                // 的引擎本体一起删掉——这条由 guard.rs 的断言在测试期检查。
                prefix: "vastplayer.dll.old",
            }],
            Self::ImageViewer | Self::RegistryAssoc => &[],
        }
    }

    /// 默认是否勾选。
    ///
    /// 这里刻意穷尽匹配全部变体：新增组件时若未显式登记默认勾选状态，编译会
    /// 失败，避免新组件在无人确认的情况下被默认勾选。当前全部默认勾选——它们
    /// 都是运行本工具就想清掉的东西；但清单本身是可选的，用户可以只清其中一部分。
    ///
    /// 清理目标在真机实测确认「移除后客户端能正常启动」之前不得默认勾选：先把它
    /// 作为独立组件，在默认不勾选的状态下实测，通过后再合并进所属组件。
    pub fn default_enabled(self) -> bool {
        match self {
            Self::ImageViewer | Self::Player | Self::RegistryAssoc => true,
        }
    }

    /// 相对安装目录的路径是否可能是本组件声明过的清理目标。
    ///
    /// 覆盖以下情况：固定目标、通配前缀展开出的条目、以及「清空目录内容」型组件
    /// 目录下的直接子项。还原时用它复核 manifest 自报的目标，因此**不能依赖
    /// 磁盘当前状态**——通配目标此时已经被搬走了，磁盘上什么也查不到。
    ///
    /// 通配前缀按 [`crate::detect::component_rels`] 的 `starts_with` 同一规则
    /// 匹配，**包括恰好等于前缀的名字**：那个名字能被搬走，就必须能被匹配到。
    pub fn matches_target(self, relative: &str) -> bool {
        for p in self.relative_paths() {
            if p.eq_ignore_ascii_case(relative) {
                return true;
            }
            if self.moves_dir_contents() && is_direct_child_of(p, relative) {
                return true;
            }
        }
        self.prefix_targets().iter().any(|pt| {
            let head = format!("{}\\{}", pt.dir, pt.prefix);
            relative
                .get(..head.len())
                .is_some_and(|h| h.eq_ignore_ascii_case(&head))
        })
    }
}

/// `relative` 是否是 `dir` 的直接子项（只比一层，不递归到孙辈）
fn is_direct_child_of(dir: &str, relative: &str) -> bool {
    let Some(head) = relative.get(..dir.len()) else {
        return false;
    };
    if !head.eq_ignore_ascii_case(dir) {
        return false;
    }
    if relative.as_bytes().get(dir.len()) != Some(&b'\\') {
        return false;
    }
    match relative.get(dir.len() + 1..) {
        Some(rest) => !rest.is_empty() && !rest.contains('\\'),
        None => false,
    }
}

/// 该相对路径是否是任一组件声明过的清理目标
pub fn is_declared_target(relative: &str) -> bool {
    ComponentId::ALL
        .iter()
        .any(|id| id.matches_target(relative))
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

    /// `self` 是否等于 `ancestor`，或位于其子树内（必须同一个根）。
    ///
    /// 用于把「备份文件里写了什么」限制在「这次允许还原的范围」之内：
    /// `.reg` 来自磁盘，可能被替换或篡改，只看键路径本身，不看来历。
    pub fn is_within(&self, ancestor: &RegPath) -> bool {
        if self.root != ancestor.root {
            return false;
        }
        // 空 sub 表示根键本身，任何同根的键都在其内
        if ancestor.sub.is_empty() || self.sub.eq_ignore_ascii_case(&ancestor.sub) {
            return true;
        }
        // 逐字符比较前缀，并确认边界是分隔符（`A` 不得匹配 `AB`）
        match self.sub.get(..ancestor.sub.len()) {
            Some(head) => {
                head.eq_ignore_ascii_case(&ancestor.sub)
                    && self.sub.as_bytes().get(ancestor.sub.len()) == Some(&b'\\')
            }
            None => false,
        }
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
            3,
            "组件数量变了：新增组件会出现在可勾选清单里，请确认新目标确实可清，再更新本断言"
        );
    }

    /// 界面只有「看图 / 播放器 / 文件关联」，与用户想移除的捆绑功能一一对应。
    ///
    /// 出现按文件粒度命名的组件（如「播放器启动器」「播放器暂存包」）就说明划分
    /// 又退回到了实现视角，用户被要求替代码做归类。
    #[test]
    fn components_are_grouped_by_user_intent() {
        assert_eq!(
            ComponentId::ALL
                .iter()
                .map(|c| c.label())
                .collect::<Vec<_>>(),
            vec!["智能看图", "智能播放器", "文件关联"]
        );
        // 播放器的目标分散在两个目录和一处通配前缀中，合并后仍是一项
        assert_eq!(ComponentId::Player.relative_paths().len(), 5);
        assert_eq!(ComponentId::Player.prefix_targets().len(), 1);
    }

    /// 同一个组件内部的清理目标，可逆性必须一致。
    ///
    /// 注册表组件单独成项就是因为 `UserChoice` 删掉后只能让系统重新询问，无法精确
    /// 还原；文件组件删掉后却能原样搬回来。把两者并成一项会让「能还原」的承诺失真。
    #[test]
    fn registry_and_file_targets_stay_apart() {
        assert!(ComponentId::RegistryAssoc.relative_paths().is_empty());
        for id in [ComponentId::ImageViewer, ComponentId::Player] {
            assert!(
                !id.relative_paths().is_empty() || !id.prefix_targets().is_empty(),
                "{id:?} 没有文件目标"
            );
        }
    }

    /// 每个组件都必须有某种目标：既没有固定路径也没有通配路径，又不是注册表组件的，
    /// 在界面上会显示成一个勾选后没有任何效果的条目。
    #[test]
    fn every_file_component_declares_a_target() {
        for id in ComponentId::ALL {
            if id == ComponentId::RegistryAssoc {
                continue;
            }
            assert!(
                !id.relative_paths().is_empty() || !id.prefix_targets().is_empty(),
                "{id:?} 既没有固定路径也没有通配目标"
            );
        }
    }

    /// 播放器组件的通配前缀必须严格长于同目录的禁区文件名。
    ///
    /// `Player` 匹配 `vastplayer.dll.old*`，而正在被宿主加载的
    /// `module\BrowserEngine\vastplayer.dll` 绝不能出现在展开结果里。
    #[test]
    fn prefix_target_cannot_swallow_the_live_engine() {
        use crate::data::Tables;
        use crate::guard;

        let t = &Tables::load().unwrap().forbidden;
        for id in ComponentId::ALL {
            for pt in id.prefix_targets() {
                // 前缀本身（尚未接上时间戳）不得是一个禁区文件
                assert!(
                    !guard::is_forbidden(t, &format!("{}\\{}", pt.dir, pt.prefix)),
                    "{id:?} 的通配前缀 {}\\{} 本身属于禁区",
                    pt.dir,
                    pt.prefix
                );
                // 接上时间戳之后的样本可清理，否则整次清理会被禁区拦截并回滚
                let sample = format!("{}\\{}20260608T084445", pt.dir, pt.prefix);
                assert!(
                    !guard::is_forbidden(t, &sample),
                    "通配目标 {sample} 被禁区拦截，清理会整次中止并回滚"
                );
            }
        }

        // 反向验证：正在使用的引擎本体仍在禁区里，通配前缀比它更长，匹配不到它
        assert!(guard::is_forbidden(
            t,
            r"module\BrowserEngine\vastplayer.dll"
        ));
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

    /// [`ComponentId::matches_target`] 与 [`crate::detect::component_rels`]
    /// 的展开规则必须一致：清理层搬走的每一项，还原时都要能被匹配到。
    ///
    /// 关键样本是恰好等于通配前缀的文件名——它会被 `starts_with` 命中并搬走，
    /// `matches_target` 若不接受相等，这份备份就永远无法还原。
    #[test]
    fn matches_target_recognizes_everything_component_rels_moves() {
        use crate::detect;

        let dir = std::env::temp_dir().join("bc_model_matches");
        let _ = std::fs::remove_dir_all(&dir);
        let be = dir.join(r"module\BrowserEngine");
        std::fs::create_dir_all(&be).unwrap();
        // 恰好等于前缀的名字，以及带时间戳的常规形态
        std::fs::write(be.join("vastplayer.dll.old"), b"x").unwrap();
        std::fs::write(be.join("vastplayer.dll.old20260608T084445"), b"x").unwrap();

        for rel in detect::component_rels(&dir, ComponentId::Player) {
            assert!(
                ComponentId::Player.matches_target(&rel),
                "component_rels 展开的 {rel} 必须能被 matches_target 匹配到"
            );
        }
        assert!(
            ComponentId::Player.matches_target(r"module\BrowserEngine\vastplayer.dll.old"),
            "恰好等于前缀的名字必须被匹配到"
        );

        // 非目标一律不匹配：同目录的其他文件与跨组件的路径
        assert!(!ComponentId::Player.matches_target(r"module\BrowserEngine\videoipc.dll"));
        assert!(!ComponentId::ImageViewer.matches_target(r"module\BrowserEngine\vastplayer.dll"));
        // 「清空目录内容」型组件：直接子项可匹配，更深层的不匹配（清理也只搬直接子项）
        assert!(ComponentId::ImageViewer.matches_target(r"module\ImageViewer\Qt5Core.dll"));
        assert!(!ComponentId::ImageViewer.matches_target(r"module\ImageViewer\sub\x.dll"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
