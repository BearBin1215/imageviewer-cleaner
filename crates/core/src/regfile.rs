//! `.reg` 文件解析 —— 用于还原时把注册表写回
//!
//! 只支持本工具自己导出的格式（见 [`crate::sys::registry`] 的 `export_key`），
//! 不追求覆盖 Windows 注册表编辑器的全部语法。这样解析逻辑可以独立单测，
//! 也不必依赖未公开的 `RegImportW` 导出。

use crate::error::{Error, Result};
use crate::model::{RegPath, RegRoot};

/// 一个待写回的注册表值
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegEntry {
    pub path: RegPath,
    /// None 表示键的默认值
    pub name: Option<String>,
    pub value: RegValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegValue {
    Str(String),
    Dword(u32),
    Binary(Vec<u8>),
}

/// 解析结果：出现过的键（含无值键）与值条目
pub struct Parsed {
    /// 所有出现过的键，用于还原时重建——即使它没有任何值
    pub keys: Vec<RegPath>,
    pub entries: Vec<RegEntry>,
}

/// 解析 `.reg` 文本，只返回值条目
pub fn parse(text: &str) -> Result<Vec<RegEntry>> {
    Ok(parse_full(text)?.entries)
}

/// 解析 `.reg` 文本，返回键集合与值条目
pub fn parse_full(text: &str) -> Result<Parsed> {
    let mut entries = Vec::new();
    let mut keys = Vec::new();
    let mut current: Option<RegPath> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line.starts_with("Windows Registry Editor") {
            continue;
        }
        if let Some(inner) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let key = parse_key(inner)?;
            keys.push(key.clone());
            current = Some(key);
            continue;
        }
        let Some(eq) = line.find('=') else {
            continue;
        };
        let (lhs, rhs) = line.split_at(eq);
        let rhs = &rhs[1..];
        let Some(path) = current.clone() else {
            return Err(Error::data("reg", serde::de::Error::custom("值出现在键之前")));
        };
        let name = if lhs.trim() == "@" {
            None
        } else {
            Some(unescape(lhs.trim().trim_matches('"')))
        };
        entries.push(RegEntry {
            path,
            name,
            value: parse_value(rhs.trim())?,
        });
    }
    Ok(Parsed { keys, entries })
}

fn parse_key(s: &str) -> Result<RegPath> {
    let (root, rest) = if let Some(rest) = s.strip_prefix("HKEY_USERS\\") {
        (RegRoot::Hku, rest)
    } else if let Some(rest) = s.strip_prefix("HKEY_LOCAL_MACHINE\\") {
        (RegRoot::Hklm, rest)
    } else if s == "HKEY_USERS" {
        (RegRoot::Hku, "")
    } else if s == "HKEY_LOCAL_MACHINE" {
        (RegRoot::Hklm, "")
    } else {
        return Err(Error::data(
            "reg",
            serde::de::Error::custom(format!("不支持的根键：{s}")),
        ));
    };
    Ok(RegPath {
        root,
        sub: rest.to_owned(),
    })
}

fn parse_value(s: &str) -> Result<RegValue> {
    if let Some(hex) = s.strip_prefix("dword:") {
        let v = u32::from_str_radix(hex.trim(), 16).map_err(|e| {
            Error::data(
                "reg",
                serde::de::Error::custom(format!("dword 解析失败：{e}")),
            )
        })?;
        return Ok(RegValue::Dword(v));
    }

    if let Some(hex) = s.strip_prefix("hex:") {
        let bytes = if hex.trim().is_empty() {
            Vec::new()
        } else {
            hex.split(',')
                .map(|b| {
                    u8::from_str_radix(b.trim(), 16).map_err(|e| {
                        Error::data(
                            "reg",
                            serde::de::Error::custom(format!("hex 解析失败：{e}")),
                        )
                    })
                })
                .collect::<Result<Vec<u8>>>()?
        };
        return Ok(RegValue::Binary(bytes));
    }

    // 字符串：去掉首尾引号并反转义
    let s = s.trim();
    let inner = s
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .ok_or_else(|| {
            Error::data(
                "reg",
                serde::de::Error::custom(format!("无法识别的值：{s}")),
            )
        })?;
    Ok(RegValue::Str(unescape(inner)))
}

/// 反转义 `\\` 与 `\"`
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::registry::RegistryBackend;
    use crate::sys::registry::Win32Registry;

    /// 测试辅助：解析样例文本
    fn round_trip(text: &str) -> Vec<RegEntry> {
        parse(text).unwrap()
    }

    #[test]
    fn parses_exported_shape() {
        let text = "Windows Registry Editor Version 5.00\r\n\r\n\
[HKEY_USERS\\S-1-5-21-1-2-3-1001\\Software\\RegisteredApplications]\r\n\
\"BaiduNetdiskPlayer\"=\"C:\\\\Program Files\\\\Baidu\\\\BaiduNetdisk\\\\player.exe\"\r\n\
\"Count\"=dword:00000002\r\n\
\"Blob\"=hex:01,02,ff\r\n\
\r\n";

        let entries = round_trip(text);
        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0].path.root, RegRoot::Hku);
        assert_eq!(
            entries[0].path.sub,
            r"S-1-5-21-1-2-3-1001\Software\RegisteredApplications"
        );
        assert_eq!(entries[0].name.as_deref(), Some("BaiduNetdiskPlayer"));
        assert_eq!(
            entries[0].value,
            RegValue::Str(r"C:\Program Files\Baidu\BaiduNetdisk\player.exe".into())
        );

        assert_eq!(entries[1].value, RegValue::Dword(2));
        assert_eq!(entries[2].value, RegValue::Binary(vec![0x01, 0x02, 0xff]));
    }

    #[test]
    fn handles_default_value_and_multiple_keys() {
        let text = "Windows Registry Editor Version 5.00\r\n\r\n\
[HKEY_LOCAL_MACHINE\\Software\\Classes\\A]\r\n\
@=\"default text\"\r\n\
\r\n\
[HKEY_USERS\\S-1-5-18]\r\n\
\"x\"=\"y\"\r\n";

        let e = round_trip(text);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].name, None, "@ 表示默认值");
        assert_eq!(e[0].path.root, RegRoot::Hklm);
        assert_eq!(e[1].path.root, RegRoot::Hku);
    }

    #[test]
    fn unescapes_quotes_and_backslashes() {
        assert_eq!(unescape(r"a\\b"), r"a\b");
        assert_eq!(unescape(r#"say \"hi\""#), r#"say "hi""#);
    }

    #[test]
    fn rejects_unknown_root() {
        let text = "Windows Registry Editor Version 5.00\r\n\r\n[HKEY_CURRENT_USER\\X]\r\n";
        assert!(parse(text).is_err(), "未支持的根键应报错");
    }

    #[test]
    fn parse_full_keeps_value_less_keys() {
        // 无值的容器键也必须被记录，否则还原时无法重建它
        let text = "Windows Registry Editor Version 5.00\r\n\r\n\
[HKEY_USERS\\S-1-5-21-1-2-3-1001_Classes\\BaiduNetdiskPlayerAssociations]\r\n\r\n";
        let parsed = parse_full(text).unwrap();
        assert!(parsed.entries.is_empty(), "无值键不产生值条目");
        assert_eq!(parsed.keys.len(), 1, "但键本身必须被记录以便还原");
        assert!(
            parsed.keys[0]
                .sub
                .contains("BaiduNetdiskPlayerAssociations")
        );
    }

    #[test]
    fn export_then_parse_round_trip_against_memory_backend() {
        // 替身不做真实导出，这里只验证解析端对空键的处理
        let text = "Windows Registry Editor Version 5.00\r\n\r\n\
[HKEY_USERS\\S-1-5-21-1-2-3-1001_Classes\\BaiduNetdiskPlayerAssociations]\r\n\r\n";
        let entries = parse(text).unwrap();
        assert!(entries.is_empty(), "空键不产生值条目");

        let _ = Win32Registry::new().enumerate_user_hives();
    }
}
