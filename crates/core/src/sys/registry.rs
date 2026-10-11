//! 注册表后端 —— trait 抽象 + Win32 实现
//!
//! `HKCR` 是 `HKLM\Software\Classes` 与**当前用户** Classes 的合并视图。
//! 工具以管理员身份运行时「当前用户」是管理员，**直接删 `HKCR\BaiduNetdisk*` 不会命中**。
//! 因此本层只接受**已解析的绝对路径**（`RegPath`），由上层显式指定 `HKU\<SID>_Classes` 等。

use std::path::Path;

use windows::Win32::Foundation::WIN32_ERROR;
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_READ, KEY_WRITE, REG_BINARY, REG_DWORD,
    REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteTreeW,
    RegDeleteValueW, RegEnumKeyExW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW,
};
use windows::core::PCWSTR;

use crate::error::{Error, Result};
use crate::model::{RegPath, RegRoot};
use crate::regfile::{RegValue, parse_full};

pub trait RegistryBackend: Send + Sync {
    /// 枚举 `HKEY_USERS` 下已加载的 hive 名（如 `S-1-5-21-...-1001`、`...-1001_Classes`）
    fn enumerate_user_hives(&self) -> Result<Vec<String>>;

    fn enum_subkeys(&self, path: &RegPath) -> Result<Vec<String>>;
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
    fn import_reg_file(&self, path: &Path) -> Result<()>;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
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

    unsafe fn open(&self, path: &RegPath, write: bool) -> Result<Option<HKEY>> {
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

    unsafe fn enum_subkeys_h(&self, hk: HKEY) -> Result<Vec<String>> {
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
            if rc.is_err() {
                break;
            }
            out.push(String::from_utf16_lossy(&name[..len as usize]));
            idx += 1;
        }
        Ok(out)
    }

    /// 枚举键下的所有 (名称, 类型, 原始数据)
    unsafe fn enum_values(&self, hk: HKEY) -> Result<Vec<(String, REG_VALUE_TYPE, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut idx = 0u32;
        loop {
            let mut name = [0u16; 512];
            let mut name_len = name.len() as u32;
            // RegEnumValueW 的 lptype 形参是 *mut u32，不是 REG_VALUE_TYPE
            let mut ty_raw: u32 = REG_SZ.0;
            let mut cb: u32 = 0;
            // 第一趟：取大小
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
            if rc.is_err() {
                break;
            }
            let mut data = vec![0u8; cb as usize + 8];
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
                break;
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
        let rc = unsafe { self.enum_subkeys_h(HKEY_USERS) }?;
        Ok(rc)
    }

    fn enum_subkeys(&self, path: &RegPath) -> Result<Vec<String>> {
        match unsafe { self.open(path, false)? } {
            None => Ok(Vec::new()),
            Some(hk) => {
                let r = unsafe { self.enum_subkeys_h(hk) };
                let _ = unsafe { RegCloseKey(hk) };
                r
            }
        }
    }

    fn read_string(&self, path: &RegPath, value: &str) -> Result<Option<String>> {
        let Some(hk) = (unsafe { self.open(path, false)? }) else {
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
        match unsafe { self.open(path, false)? } {
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
        let Some(hk) = (unsafe { self.open(path, true)? }) else {
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
        let Some(hk) = (unsafe { self.open(path, false)? }) else {
            return Ok(()); // 键不存在，无需导出
        };
        let mut text = String::from("Windows Registry Editor Version 5.00\r\n\r\n");
        let r = unsafe { self.export_recursive(hk, path, &mut text) };
        let _ = unsafe { RegCloseKey(hk) };
        r?;
        std::fs::write(out, text.as_bytes()).map_err(|e| Error::io(out, e))
    }

    fn set_value(&self, path: &RegPath, name: Option<&str>, value: &RegValue) -> Result<()> {
        self.create_key(path)?;
        let Some(hk) = (unsafe { self.open(path, true)? }) else {
            return Err(Error::io(
                std::path::PathBuf::from(path.full()),
                std::io::Error::other("创建键之后仍无法打开"),
            ));
        };

        let (ty, data): (REG_VALUE_TYPE, Vec<u8>) = match value {
            RegValue::Str(s) => {
                let mut bytes: Vec<u8> = Vec::with_capacity(s.len() * 2 + 2);
                for u in s.encode_utf16().chain(std::iter::once(0)) {
                    bytes.extend_from_slice(&u.to_le_bytes());
                }
                (REG_SZ, bytes)
            }
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

    fn import_reg_file(&self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let parsed = parse_full(&text)?;
        // 先重建所有键（含无值的容器键），再写回各值
        for key in &parsed.keys {
            self.create_key(key)?;
        }
        for entry in parsed.entries {
            self.set_value(&entry.path, entry.name.as_deref(), &entry.value)?;
        }
        Ok(())
    }
}

impl Win32Registry {
    /// 递归导出为 .reg 文本
    unsafe fn export_recursive(&self, hk: HKEY, path: &RegPath, out: &mut String) -> Result<()> {
        out.push_str(&format!(
            "[{}]\r\n",
            path.full()
                .replace("HKU\\", "HKEY_USERS\\")
                .replace("HKLM\\", "HKEY_LOCAL_MACHINE\\")
        ));

        for (name, ty, data) in unsafe { self.enum_values(hk) }? {
            let rendered = render_value(ty, &data);
            if name.is_empty() {
                out.push_str(&format!("@={rendered}\r\n"));
            } else {
                out.push_str(&format!(
                    "\"{}\"={rendered}\r\n",
                    name.replace('\\', "\\\\")
                ));
            }
        }
        out.push_str("\r\n");

        for sub in unsafe { self.enum_subkeys_h(hk) }? {
            let mut child = HKEY::default();
            let sn = wide(&sub);
            let rc =
                unsafe { RegOpenKeyExW(hk, PCWSTR(sn.as_ptr()), Some(0), KEY_READ, &mut child) };
            if rc.is_err() {
                continue;
            }
            let child_path = path.join(&sub);
            let r = unsafe { self.export_recursive(child, &child_path, out) };
            let _ = unsafe { RegCloseKey(child) };
            r?;
        }
        Ok(())
    }
}

/// 把原始值渲染成 .reg 文本
fn render_value(ty: REG_VALUE_TYPE, data: &[u8]) -> String {
    match ty {
        REG_SZ | windows::Win32::System::Registry::REG_EXPAND_SZ => {
            let u16s: Vec<u16> = data
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&c| c != 0)
                .collect();
            let s = String::from_utf16_lossy(&u16s);
            format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
        }
        windows::Win32::System::Registry::REG_DWORD => {
            let v = data
                .get(..4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .unwrap_or(0);
            format!("dword:{v:08x}")
        }
        _ => {
            let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
            format!("hex:{hex}")
        }
    }
}


