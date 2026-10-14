//! 外置数据表（编译期嵌入）—— 网盘改名/改路径时只需改 JSON，无需改逻辑

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::model::{ComponentId, PlaceholderKind, RegItem, RegPath};

const PATHS_JSON: &str = include_str!("../../../data/paths.json");
const REGISTRY_JSON: &str = include_str!("../../../data/registry.json");
const EXTENSIONS_JSON: &str = include_str!("../../../data/extensions.json");
const PROCESSES_JSON: &str = include_str!("../../../data/processes.json");
const FORBIDDEN_JSON: &str = include_str!("../../../data/forbidden.json");

#[derive(Debug, Clone, Deserialize)]
pub struct PathsTable {
    pub marker: String,
    pub candidates: Vec<String>,
    pub install_dir_registry_value: RegistryValueRef,
    /// 清理后必须仍然存在的主客户端文件（相对安装目录）
    pub required_files: Vec<String>,
    /// 需要放置占位物的组件
    pub placeholder_targets: Vec<PlaceholderTarget>,
}

/// 一个占位目标：组件清理后，在某个清理目标的原位置放置相反类型的同名对象
#[derive(Debug, Clone, Deserialize)]
pub struct PlaceholderTarget {
    pub component: ComponentId,
    /// 占位对象所在的相对路径，必须是该组件 `relative_paths()` 里的一项
    ///
    /// 组件合并之后，「第一项」不再能代表要占位的目标（一个组件有多个路径，
    /// 其中只有入口点需要占位），所以这里点名具体路径。
    pub relative_path: String,
    pub kind: PlaceholderKind,
}

impl PathsTable {
    /// 某个相对路径是否为占位目标
    pub fn placeholder_kind_for(&self, relative: &str) -> Option<PlaceholderKind> {
        self.placeholder_targets
            .iter()
            .find(|t| t.relative_path.eq_ignore_ascii_case(relative))
            .map(|t| t.kind)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegistryValueRef {
    pub sub: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegistryTable {
    pub user_classes_keys: Vec<String>,
    pub user_software_keys: Vec<String>,
    pub registered_applications_key: String,
    pub registered_applications_values: Vec<String>,
    pub open_with_list_entries: Vec<String>,
    pub machine_classes_keys: Vec<String>,
    pub machine_applications_keys: Vec<String>,
    pub userchoice_relative: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtensionsTable {
    pub video: Vec<String>,
    pub image: Vec<String>,
    pub audio: Vec<String>,
}

impl ExtensionsTable {
    /// 全部扩展名（含前导点）
    pub fn all(&self) -> Vec<&str> {
        self.video
            .iter()
            .chain(&self.image)
            .chain(&self.audio)
            .map(String::as_str)
            .collect()
    }

    pub fn count(&self) -> usize {
        self.video.len() + self.image.len() + self.audio.len()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProcessesTable {
    #[serde(rename = "match")]
    pub match_mode: String,
    pub graceful_timeout_ms: u64,
    pub targets: Vec<String>,
    pub check_services: bool,
}

impl ProcessesTable {
    /// 严格精确匹配（`match` 必须为 "exact"，防止误用子串匹配）
    pub fn is_target(&self, exe_name: &str) -> bool {
        debug_assert_eq!(self.match_mode, "exact");
        self.targets
            .iter()
            .any(|t| t.eq_ignore_ascii_case(exe_name))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ForbiddenTable {
    pub relative_paths: Vec<String>,
}

pub struct Tables {
    pub paths: PathsTable,
    pub registry: RegistryTable,
    pub extensions: ExtensionsTable,
    pub processes: ProcessesTable,
    pub forbidden: ForbiddenTable,
}

impl Tables {
    pub fn load() -> Result<Self> {
        Ok(Self {
            paths: parse("paths.json", PATHS_JSON)?,
            registry: parse("registry.json", REGISTRY_JSON)?,
            extensions: parse("extensions.json", EXTENSIONS_JSON)?,
            processes: parse("processes.json", PROCESSES_JSON)?,
            forbidden: parse("forbidden.json", FORBIDDEN_JSON)?,
        })
    }

    /// 把数据表展开成「目标用户」层面的具体注册表项
    ///
    /// 类注册键在 `HKU\<SID>_Classes`，不在 HKLM。
    pub fn build_user_registry_items(&self, sid: &str) -> Vec<RegItem> {
        let classes = RegPath::user_classes(sid);
        let user = RegPath::user_software(sid);
        let mut out = Vec::new();

        for k in &self.registry.user_classes_keys {
            out.push(RegItem {
                path: classes.join(k),
                value: None,
            });
        }
        for k in &self.registry.user_software_keys {
            out.push(RegItem {
                path: user.join(k),
                value: None,
            });
        }
        let reg_apps = user.join(&self.registry.registered_applications_key);
        for v in &self.registry.registered_applications_values {
            out.push(RegItem {
                path: reg_apps.clone(),
                value: Some(v.clone()),
            });
        }
        out
    }

    /// HKLM 层（防御性：其他版本可能写全局）
    pub fn build_machine_registry_items(&self) -> Vec<RegItem> {
        let classes = RegPath::machine_classes();
        let mut out = Vec::new();
        for k in &self.registry.machine_classes_keys {
            out.push(RegItem {
                path: classes.join(k),
                value: None,
            });
        }
        for k in &self.registry.machine_applications_keys {
            out.push(RegItem {
                path: classes.join("Applications").join(k),
                value: None,
            });
        }
        out
    }

    /// 某扩展名下的 OpenWithList 候选条目（全路径）
    pub fn open_with_list_path(&self, sid: &str, ext: &str) -> RegPath {
        RegPath::user_classes(sid).join(ext).join("OpenWithList")
    }

    /// UserChoice 子键路径（删除整个子键，不修改其中的值）
    pub fn user_choice_path(&self, sid: &str, ext: &str) -> RegPath {
        RegPath::user_software(sid)
            .join(&self.registry.userchoice_relative)
            .join(ext)
            .join("UserChoice")
    }
}

fn parse<T: for<'de> Deserialize<'de>>(name: &str, src: &str) -> Result<T> {
    serde_json::from_str(src).map_err(|e| Error::data(name, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-111-222-333-1001";

    #[test]
    fn tables_load_and_are_non_trivial() {
        let t = Tables::load().expect("数据表应能解析");
        assert!(t.paths.candidates.len() >= 5);
        assert_eq!(t.paths.marker, "BaiduNetdisk.exe");
        assert!(t.extensions.count() > 90, "扩展名应覆盖 ~100 项");
        assert_eq!(t.processes.match_mode, "exact");
        assert!(
            t.processes
                .targets
                .contains(&"BaiduNetdiskUnite.exe".to_owned())
        );
        assert!(!t.forbidden.relative_paths.is_empty());
    }

    #[test]
    fn user_registry_items_target_user_classes_not_hklm() {
        let t = Tables::load().unwrap();
        let items = t.build_user_registry_items(SID);
        assert!(!items.is_empty());

        // 类注册键必须在 <SID>_Classes
        let classes_key = items
            .iter()
            .find(|i| i.path.sub.contains("BaiduNetdiskImageViewerAssociations"))
            .expect("应包含 ImageViewerAssociations");
        assert!(
            classes_key.path.sub.starts_with(&format!("{SID}_Classes")),
            "关联键必须在 <SID>_Classes，实际: {}",
            classes_key.path.sub
        );
        assert_eq!(classes_key.path.root, crate::model::RegRoot::Hku);
        assert!(classes_key.value.is_none(), "整键删除，不带值名");

        // RegisteredApplications 只删除值，不删除整个键
        let reg_app = items
            .iter()
            .find(|i| i.path.sub.contains("RegisteredApplications"))
            .expect("应包含 RegisteredApplications");
        assert_eq!(reg_app.value.as_deref(), Some("BaiduNetdiskImageViewer"));
    }

    #[test]
    fn machine_items_never_use_substring_matching() {
        let t = Tables::load().unwrap();
        for i in t.build_machine_registry_items() {
            // 严禁出现 Baiduyunguanjia（另一个产品）
            assert!(
                !i.path.sub.to_ascii_lowercase().contains("yunguanjia"),
                "不得触碰 BaiduYunGuanjia: {}",
                i.path.sub
            );
        }
    }

    #[test]
    fn user_choice_path_is_built_correctly() {
        let t = Tables::load().unwrap();
        let p = t.user_choice_path(SID, ".jpg");
        assert_eq!(
            p.full(),
            format!(
                "HKU\\{SID}\\Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\.jpg\\UserChoice"
            )
        );
    }

    #[test]
    fn process_matching_is_exact_not_substring() {
        let t = Tables::load().unwrap();
        assert!(t.processes.is_target("BaiduNetdisk.exe"));
        assert!(t.processes.is_target("baidunetdisk.exe"), "大小写不敏感");
        // 子串匹配会误杀这些
        assert!(!t.processes.is_target("BaiduYunGuanjia.exe"));
        assert!(!t.processes.is_target("MyBaiduHelper.exe"));
        assert!(!t.processes.is_target("yun.exe"));
    }

    /// 候选顺序即兜底扫描的优先级：多个候选同时含 marker 文件时取最前面的。
    /// 用户目录候选刻意排在 Program Files 之前——同时存在用户级与系统级安装时
    /// 优先选中用户级。调整顺序会改变这类机器上选中的安装目录，须有意为之。
    #[test]
    fn candidate_order_prioritizes_user_level_installs() {
        let t = Tables::load().unwrap();
        assert!(
            t.paths
                .candidates
                .iter()
                .take(2)
                .all(|c| c.contains("APPDATA")),
            "前两条应是用户目录候选，实际 {:?}",
            &t.paths.candidates[..2]
        );
    }
}
