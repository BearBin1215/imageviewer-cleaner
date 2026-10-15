//! 注册表后端 —— trait 抽象 + Win32 实现
//!
//! `HKCR` 是 `HKLM\Software\Classes` 与当前用户 Classes 的合并视图。
//! 工具以管理员身份运行时，「当前用户」是管理员，因此删除 `HKCR\BaiduNetdisk*`
//! 命不中目标。本层只接受已解析的绝对路径（`RegPath`），由上层显式指定
//! `HKU\<SID>_Classes` 等具体位置。

use std::path::Path;

use windows::Win32::Foundation::{ERROR_NO_MORE_ITEMS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_READ, KEY_WRITE, REG_BINARY, REG_DWORD,
    REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW,
    RegDeleteTreeW, RegDeleteValueW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW,
};
use windows::core::PCWSTR;

use crate::error::{Error, Result};
use crate::model::{RegPath, RegRoot};
use crate::sys::regfile::{Parsed, RegValue, check_scope, decode_utf16, parse_full};
use crate::sys::wide;

pub trait RegistryBackend: Send + Sync {
    /// 枚举 `HKEY_USERS` 下已加载的 hive 名（如 `S-1-5-21-...-1001`、`...-1001_Classes`）
    fn enumerate_user_hives(&self) -> Result<Vec<String>>;

    fn read_string(&self, path: &RegPath, value: &str) -> Result<Option<String>>;
    fn key_exists(&self, path: &RegPath) -> Result<bool>;
    /// 删除整个键（含子键）
    fn delete_key(&self, path: &RegPath) -> Result<()>;
    fn delete_value(&self, path: &RegPath, value: &str) -> Result<()>;
    /// 递归导出为 `.reg` 文本（用户可读、可手工恢复）
    fn export_key(&self, path: &RegPath, out: &Path) -> Result<()>;
    /// 确保键存在（还原时用）
    fn create_key(&self, path: &RegPath) -> Result<()>;
    /// 写入一个值（还原时用）
    fn set_value(&self, path: &RegPath, name: Option<&str>, value: &RegValue) -> Result<()>;
    /// 从 `.reg` 文件还原
    ///
    /// `scope` 是本次导入允许触碰的键：文件里解析出的每个键都必须等于它，
    /// 或位于它的子树内。备份文件来自磁盘，可能被替换或篡改，越界的键一律
    /// **整份拒绝**（不做部分导入），调用方按失败处理即可。
    fn import_reg_file(&self, path: &Path, scope: &RegPath) -> Result<()>;
}

/// 把解析出的 `.reg` 内容写回后端：先重建所有键（含无值的容器键），再写回各值。
///
/// 真实实现与测试替身共用，避免两边的写入顺序出现差异。
pub(crate) fn apply_parsed(backend: &dyn RegistryBackend, parsed: &Parsed) -> Result<()> {
    for key in &parsed.keys {
        backend.create_key(key)?;
    }
    for entry in &parsed.entries {
        backend.set_value(&entry.path, entry.name.as_deref(), &entry.value)?;
    }
    Ok(())
}

fn root_handle(root: RegRoot) -> HKEY {
    match root {
        RegRoot::Hku => HKEY_USERS,
        RegRoot::Hklm => HKEY_LOCAL_MACHINE,
    }
}

fn err(path: &RegPath, source: WIN32_ERROR) -> Error {
    Error::Registry {
        hive: path.root_name().to_owned(),
        key: path.sub.clone(),
        source: source.to_hresult().into(),
    }
}

// ───────────────────────────────── Win32 实现 ─────────────────────────────────

pub struct Win32Registry;

impl Win32Registry {
    pub fn new() -> Self {
        Self
    }

    fn open(&self, path: &RegPath, write: bool) -> Result<Option<HKEY>> {
        let mut hk = HKEY::default();
        let sub = wide(&path.sub);
        let access = if write { KEY_WRITE } else { KEY_READ };
        let rc = unsafe {
            RegOpenKeyExW(
                root_handle(path.root),
                PCWSTR(sub.as_ptr()),
                Some(0),
                access,
                &mut hk,
            )
        };
        if rc.is_err() {
            return Ok(None);
        }
        Ok(Some(hk))
    }

    fn enum_subkeys_h(&self, hk: HKEY) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut idx = 0u32;
        loop {
            let mut name = [0u16; 512];
            let mut len = name.len() as u32;
            let rc = unsafe {
                RegEnumKeyExW(
                    hk,
                    idx,
                    Some(windows::core::PWSTR(name.as_mut_ptr())),
                    &mut len,
                    None,
                    None,
                    None,
                    None,
                )
            };
            // 只有「没有更多项」才是正常结束；其他错误必须上报，
            // 否则备份会静默漏掉子键
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc.is_err() {
                return Err(Error::win32("RegEnumKeyExW", rc.to_hresult().into()));
            }
            out.push(String::from_utf16_lossy(&name[..len as usize]));
            idx += 1;
        }
        Ok(out)
    }

    /// 枚举键下的所有 (名称, 类型, 原始数据)
    ///
    /// 除「没有更多项」之外的错误一律向上传播：备份宁可失败，也不能静默截断。
    fn enum_values(&self, hk: HKEY) -> Result<Vec<(String, REG_VALUE_TYPE, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut idx = 0u32;
        loop {
            let mut name = [0u16; 512];
            let mut ty_raw: u32 = REG_SZ.0;
            let mut cb: u32 = 0;
            let mut name_len = name.len() as u32;
            let rc = unsafe {
                RegEnumValueW(
                    hk,
                    idx,
                    Some(windows::core::PWSTR(name.as_mut_ptr())),
                    &mut name_len,
                    None,
                    Some(&mut ty_raw),
                    None,
                    Some(&mut cb),
                )
            };
            if rc == ERROR_NO_MORE_ITEMS {
                break;
            }
            if rc.is_err() {
                return Err(Error::win32("RegEnumValueW", rc.to_hresult().into()));
            }

            // `lpcchValueName` 与 `lpcbData` 都是 in/out 参数：入参是缓冲区容量，
            // 出参是实际长度。第二趟必须重新写入容量，沿用第一趟写回的长度会让
            // 调用恒以 `ERROR_MORE_DATA` 失败
            let mut data = vec![0u8; cb as usize + 8];
            let mut name_len = name.len() as u32;
            let mut cb2 = data.len() as u32;
            let rc = unsafe {
                RegEnumValueW(
                    hk,
                    idx,
                    Some(windows::core::PWSTR(name.as_mut_ptr())),
                    &mut name_len,
                    None,
                    Some(&mut ty_raw),
                    Some(data.as_mut_ptr()),
                    Some(&mut cb2),
                )
            };
            if rc.is_err() {
                return Err(Error::win32("RegEnumValueW", rc.to_hresult().into()));
            }
            data.truncate(cb2 as usize);
            out.push((
                String::from_utf16_lossy(&name[..name_len as usize]),
                REG_VALUE_TYPE(ty_raw),
                data,
            ));
            idx += 1;
        }
        Ok(out)
    }
}

impl Default for Win32Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryBackend for Win32Registry {
    fn enumerate_user_hives(&self) -> Result<Vec<String>> {
        self.enum_subkeys_h(HKEY_USERS)
    }

    fn read_string(&self, path: &RegPath, value: &str) -> Result<Option<String>> {
        let Some(hk) = self.open(path, false)? else {
            return Ok(None);
        };
        let vn = wide(value);
        let mut ty = REG_SZ;
        let mut cb: u32 = 0;
        let _ = unsafe {
            RegQueryValueExW(
                hk,
                PCWSTR(vn.as_ptr()),
                None,
                Some(&mut ty),
                None,
                Some(&mut cb),
            )
        };
        if cb == 0 {
            let _ = unsafe { RegCloseKey(hk) };
            return Ok(None);
        }
        let mut buf = vec![0u8; cb as usize + 2];
        let mut cb2 = buf.len() as u32;
        let ok = unsafe {
            RegQueryValueExW(
                hk,
                PCWSTR(vn.as_ptr()),
                None,
                Some(&mut ty),
                Some(buf.as_mut_ptr()),
                Some(&mut cb2),
            )
        }
        .is_ok();
        let _ = unsafe { RegCloseKey(hk) };
        if !ok {
            return Ok(None);
        }
        let u16s: Vec<u16> = buf
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();
        Ok(Some(String::from_utf16_lossy(&u16s)))
    }

    fn key_exists(&self, path: &RegPath) -> Result<bool> {
        match self.open(path, false)? {
            None => Ok(false),
            Some(hk) => {
                let _ = unsafe { RegCloseKey(hk) };
                Ok(true)
            }
        }
    }

    fn delete_key(&self, path: &RegPath) -> Result<()> {
        if path.sub.is_empty() {
            return Ok(()); // 拒绝删根
        }
        let sub = wide(&path.sub);
        // RegDeleteTreeW 递归删除，无需先清空
        let rc = unsafe { RegDeleteTreeW(root_handle(path.root), PCWSTR(sub.as_ptr())) };
        if rc.is_err() {
            // 键不存在视为成功（幂等）
            if !self.key_exists(path)? {
                return Ok(());
            }
            return Err(err(path, rc));
        }
        Ok(())
    }

    fn delete_value(&self, path: &RegPath, value: &str) -> Result<()> {
        let Some(hk) = self.open(path, true)? else {
            return Ok(()); // 键不存在，幂等成功
        };
        let vn = wide(value);
        let rc = unsafe { RegDeleteValueW(hk, PCWSTR(vn.as_ptr())) };
        let _ = unsafe { RegCloseKey(hk) };
        if rc.is_err() {
            return Err(err(path, rc));
        }
        Ok(())
    }

    fn create_key(&self, path: &RegPath) -> Result<()> {
        let mut hk = HKEY::default();
        let sub = wide(&path.sub);
        let rc = unsafe {
            RegCreateKeyExW(
                root_handle(path.root),
                PCWSTR(sub.as_ptr()),
                Some(0),
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut hk,
                None,
            )
        };
        if rc.is_err() {
            return Err(err(path, rc));
        }
        let _ = unsafe { RegCloseKey(hk) };
        Ok(())
    }

    fn export_key(&self, path: &RegPath, out: &Path) -> Result<()> {
        let Some(hk) = self.open(path, false)? else {
            return Ok(()); // 键不存在，无需导出
        };
        let mut text = String::from("Windows Registry Editor Version 5.00\r\n\r\n");
        let r = self.export_recursive(hk, path, &mut text);
        let _ = unsafe { RegCloseKey(hk) };
        r?;
        std::fs::write(out, text.as_bytes()).map_err(|e| Error::io(out, e))
    }

    fn set_value(&self, path: &RegPath, name: Option<&str>, value: &RegValue) -> Result<()> {
        self.create_key(path)?;
        let Some(hk) = self.open(path, true)? else {
            return Err(Error::io(
                std::path::PathBuf::from(path.full()),
                std::io::Error::other("创建键之后仍无法打开"),
            ));
        };

        let (ty, data): (REG_VALUE_TYPE, Vec<u8>) = match value {
            RegValue::Str(s) => (REG_SZ, utf16_bytes(s)),
            // 可扩展字符串必须按原类型写回，否则 `%VAR%` 形式的命令路径会失效
            RegValue::ExpandStr(s) => (REG_EXPAND_SZ, utf16_bytes(s)),
            RegValue::Dword(v) => (REG_DWORD, v.to_le_bytes().to_vec()),
            RegValue::Binary(b) => (REG_BINARY, b.clone()),
        };

        let name_w = name.map(wide);
        let name_ptr = name_w
            .as_ref()
            .map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
        let rc = unsafe { RegSetValueExW(hk, name_ptr, Some(0), ty, Some(&data)) };
        let _ = unsafe { RegCloseKey(hk) };
        if rc.is_err() {
            return Err(err(path, rc));
        }
        Ok(())
    }

    fn import_reg_file(&self, path: &Path, scope: &RegPath) -> Result<()> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let parsed = parse_full(&text)?;

        // 先整体校验，再落盘。任何一处越界都拒绝整份文件：
        // 「一半合法、一半越界」的备份被部分写入，比整份失败更难排查
        check_scope(&parsed, scope)
            .map_err(|e| Error::RegFile(format!("{}：{e}", path.display())))?;

        apply_parsed(self, &parsed)
    }
}

impl Win32Registry {
    /// 递归导出为 `.reg` 文本
    ///
    /// 写出的内容必须能被 [`crate::sys::regfile::parse_full`] 原样读回，因此键名与
    /// 值名都要先校验；不合法时直接报错，不静默跳过。
    fn export_recursive(&self, hk: HKEY, path: &RegPath, out: &mut String) -> Result<()> {
        check_key_path(path)?;
        out.push_str(&format!("[{}]\r\n", reg_root_prefix(&path.full())));

        for (name, ty, data) in self.enum_values(hk)? {
            let rendered = render_value(ty, &data);
            if name.is_empty() {
                out.push_str(&format!("@={rendered}\r\n"));
            } else {
                check_value_name(&name)?;
                out.push_str(&format!("\"{}\"={rendered}\r\n", escape_reg_string(&name)));
            }
        }
        out.push_str("\r\n");

        for sub in self.enum_subkeys_h(hk)? {
            let mut child = HKEY::default();
            let sn = wide(&sub);
            let rc =
                unsafe { RegOpenKeyExW(hk, PCWSTR(sn.as_ptr()), Some(0), KEY_READ, &mut child) };
            if rc.is_err() {
                // 打不开子键就等于无法备份它的内容，此时不能假装导出成功
                return Err(Error::win32(
                    format!("RegOpenKeyExW({})", path.join(&sub).full()),
                    rc.to_hresult().into(),
                ));
            }
            let child_path = path.join(&sub);
            let r = self.export_recursive(child, &child_path, out);
            let _ = unsafe { RegCloseKey(child) };
            r?;
        }
        Ok(())
    }
}

/// `.reg` 文件里的根名。内部用短名（`HKU` / `HKLM`），文件里用完整名。
///
/// 只替换开头那一次：子键名里恰好出现 `HKU\` / `HKLM\` 时不应被一起改写。
fn reg_root_prefix(full: &str) -> String {
    if let Some(rest) = full.strip_prefix("HKU\\") {
        format!("HKEY_USERS\\{rest}")
    } else if let Some(rest) = full.strip_prefix("HKLM\\") {
        format!("HKEY_LOCAL_MACHINE\\{rest}")
    } else if full == "HKU" {
        "HKEY_USERS".to_owned()
    } else if full == "HKLM" {
        "HKEY_LOCAL_MACHINE".to_owned()
    } else {
        full.to_owned()
    }
}

/// 键路径里任何一段含换行或空字符，都会破坏「一行一个键」的结构
fn check_key_path(path: &RegPath) -> Result<()> {
    if let Some(c) = path.sub.chars().find(|c| matches!(c, '\r' | '\n' | '\0')) {
        return Err(Error::RegFile(format!(
            "键名含无法写进 .reg 的字符 {c:?}：{}",
            path.full()
        )));
    }
    Ok(())
}

/// 值名除换行外还禁止 `=`：解析器按第一个 `=` 切分，含 `=` 的名字读不回来
fn check_value_name(name: &str) -> Result<()> {
    if let Some(c) = name.chars().find(|c| matches!(c, '\r' | '\n' | '\0' | '=')) {
        return Err(Error::RegFile(format!(
            "值名含无法写进 .reg 的字符 {c:?}：{name:?}"
        )));
    }
    Ok(())
}

/// 转义 `.reg` 里写在引号中的文本（值名与字符串数据共用），与
/// `regfile::unescape` 对称
fn escape_reg_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 字节 → `aa,bb,cc`
fn hex_bytes(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// 字符串 → UTF-16LE 字节（含结尾 0）
fn utf16_bytes(s: &str) -> Vec<u8> {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len() * 2 + 2);
    for u in s.encode_utf16().chain(std::iter::once(0)) {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes
}

/// 把原始值渲染成 `.reg` 文本
fn render_value(ty: REG_VALUE_TYPE, data: &[u8]) -> String {
    match ty {
        // 含换行的字符串用 hex(1)（reg.exe 对 REG_SZ 的十六进制写法）：
        // 引号写法里的换行会把一行值断成两行，读不回来
        REG_SZ => {
            let s = decode_utf16(data);
            if s.contains('\r') || s.contains('\n') {
                format!("hex(1):{}", hex_bytes(data))
            } else {
                format!("\"{}\"", escape_reg_string(&s))
            }
        }
        // 可扩展字符串用 `.reg` 的 hex(2) 形式，才能把类型原样带回；
        // 用普通字符串写法会让还原后的值丢掉 REG_EXPAND_SZ，`%VAR%` 不再展开
        REG_EXPAND_SZ => format!("hex(2):{}", hex_bytes(data)),
        REG_DWORD => {
            let v = data
                .get(..4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .unwrap_or(0);
            format!("dword:{v:08x}")
        }
        _ => format!("hex:{}", hex_bytes(data)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `enum_values` 的回归点：真实注册表里含值的键必须能读出名字与数据。
    ///
    /// 其余测试都走 `InMemoryRegistry` 替身，所以这里直接读真实注册表。
    #[test]
    fn enum_values_reads_real_key_values() {
        let r = Win32Registry::new();
        let path = RegPath::hklm(r"SOFTWARE\Microsoft\Windows\CurrentVersion");
        let Some(hk) = r.open(&path, false).unwrap() else {
            println!("跳过：当前环境读不到该键");
            return;
        };
        let vals = r.enum_values(hk).unwrap();
        let _ = unsafe { RegCloseKey(hk) };

        assert!(!vals.is_empty(), "该键含值，枚举不应为空");
        assert!(
            vals.iter().any(|(n, _, _)| !n.is_empty()),
            "应读出至少一个命名值"
        );
        assert!(
            vals.iter().any(|(_, _, d)| !d.is_empty()),
            "应读出至少一份数据"
        );
    }

    #[test]
    fn export_escapes_names_and_rejects_unrepresentable_ones() {
        // 引号与反斜杠要转义，且能被 regfile::unescape 原样读回
        assert_eq!(escape_reg_string(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_reg_string(r"a\b"), r"a\\b");

        // 换行会凭空多出一行，`=` 会让名字被从中间切断，两者都必须拒绝，
        // 不静默跳过（静默跳过的备份不是完整备份）
        assert!(check_value_name("a\r\n[HKEY_LOCAL_MACHINE\\x]").is_err());
        assert!(check_value_name("a=b").is_err());
        assert!(check_value_name("normal name").is_ok());

        assert!(check_key_path(&RegPath::hklm("a\r\nb")).is_err());
        assert!(check_key_path(&RegPath::hklm(r"Software\Classes\Baidu")).is_ok());
    }

    /// 数据含换行的 REG_SZ 必须走 hex(1)：引号写法会把一行值断成两行，读不回来
    #[test]
    fn string_values_with_line_breaks_round_trip_through_hex1() {
        let rendered = render_value(REG_SZ, &utf16_bytes("a\r\nb"));
        assert!(rendered.starts_with("hex(1):"), "{rendered}");

        let text = "Windows Registry Editor Version 5.00\r\n\r\n\
[HKEY_USERS\\S-1]\r\n\
\"x\"="
            .to_owned()
            + &rendered
            + "\r\n";
        let parsed = crate::sys::regfile::parse_full(&text).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].value, RegValue::Str("a\r\nb".into()));

        // 不含换行的普通字符串仍走引号写法
        assert_eq!(render_value(REG_SZ, &utf16_bytes("plain")), "\"plain\"");
    }

    #[test]
    fn reg_root_prefix_only_rewrites_the_head() {
        assert_eq!(reg_root_prefix(r"HKU\S-1-5-18"), r"HKEY_USERS\S-1-5-18");
        assert_eq!(reg_root_prefix("HKLM"), "HKEY_LOCAL_MACHINE");
        // 子键名里恰好出现 `HKLM\` 时不得被一起改写
        assert_eq!(reg_root_prefix(r"HKU\S-1\HKLM\x"), r"HKEY_USERS\S-1\HKLM\x");
    }

    /// 越界的备份必须**整份**拒绝，且在拒绝之前不得写入任何键
    #[test]
    fn import_rejects_out_of_scope_before_writing() {
        let dir = std::env::temp_dir().join("bc_reg_scope");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("evil.reg");
        // 构造一份「越界」的备份：内容是 HKLM 下的 Run 键
        let mut text = String::from("Windows Registry Editor Version 5.00\r\n\r\n");
        text.push_str(
            "[HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run]\r\n",
        );
        text.push_str("\"BcPocScope\"=\"C:\\\\nope.exe\"\r\n");
        std::fs::write(&f, text).unwrap();

        let r = Win32Registry::new();
        let scope = RegPath::hku(r"S-1-5-21-1-2-3-1001_Classes\BaiduNetdiskPlayerAssociations");
        assert!(
            r.import_reg_file(&f, &scope).is_err(),
            "作用域之外的键应被拒绝"
        );

        // 拒绝之后不得留下任何写入痕迹
        let probe = RegPath::hklm(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run");
        assert_eq!(
            r.read_string(&probe, "BcPocScope").unwrap(),
            None,
            "拒绝整份文件后不得留下任何写入"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
