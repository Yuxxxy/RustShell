//! 密码安全存储（记住密码，M3）。

use anyhow::Result;

/// 附加熵：绑定应用，防其他程序用 DPAPI 解出同名数据
const ENTROPY: &[u8] = b"RustShell::stored_password::v1";

// Base64（标准字母表，自实现避免新增依赖）

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'\n' && c != b'\r').collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 4 {
            return None;
        }
        let mut n = 0u32;
        let mut pad = 0;
        for (i, &c) in chunk.iter().enumerate() {
            if c == b'=' {
                pad += 1;
                n <<= 6;
            } else {
                if pad > 0 {
                    return None; // '=' 后不允许再有数据
                }
                n = (n << 6) | val(c)?;
            }
            let _ = i;
        }
        let _ = pad;
        out.push((n >> 16) as u8);
        if chunk[2] != b'=' {
            out.push((n >> 8) as u8);
        }
        if chunk[3] != b'=' {
            out.push(n as u8);
        }
    }
    Some(out)
}

// DPAPI（Windows）

#[cfg(windows)]
fn dpapi_protect(plain: &str) -> Option<Vec<u8>> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Blob {
        cb_data: u32,
        pb_data: *mut u8,
    }
    #[link(name = "crypt32")]
    extern "system" {
        fn CryptProtectData(
            data_in: *const Blob,
            data_descr: *const u16,
            optional_entropy: *const Blob,
            reserved: *const c_void,
            prompt: *const c_void,
            flags: u32,
            data_out: *mut Blob,
        ) -> i32;
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    let entropy = Blob {
        cb_data: ENTROPY.len() as u32,
        pb_data: ENTROPY.as_ptr() as *mut u8,
    };
    let input = Blob {
        cb_data: plain.len() as u32,
        pb_data: plain.as_ptr() as *mut u8,
    };
    let mut out = Blob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut out,
        )
    };
    if ok == 0 {
        return None;
    }
    let slice = unsafe { std::slice::from_raw_parts(out.pb_data, out.cb_data as usize) };
    let v = slice.to_vec();
    unsafe { LocalFree(out.pb_data as *mut c_void) };
    Some(v)
}

#[cfg(windows)]
fn dpapi_unprotect(stored: &str) -> Option<Vec<u8>> {
    use std::ffi::c_void;
    #[repr(C)]
    struct Blob {
        cb_data: u32,
        pb_data: *mut u8,
    }
    #[link(name = "crypt32")]
    extern "system" {
        fn CryptUnprotectData(
            data_in: *const Blob,
            ppsz_descr: *mut *mut u16,
            optional_entropy: *const Blob,
            reserved: *const c_void,
            prompt: *const c_void,
            flags: u32,
            data_out: *mut Blob,
        ) -> i32;
        fn LocalFree(mem: *mut c_void) -> *mut c_void;
    }

    let cipher = b64_decode(stored)?;
    let entropy = Blob {
        cb_data: ENTROPY.len() as u32,
        pb_data: ENTROPY.as_ptr() as *mut u8,
    };
    let input = Blob {
        cb_data: cipher.len() as u32,
        pb_data: cipher.as_ptr() as *mut u8,
    };
    let mut out = Blob {
        cb_data: 0,
        pb_data: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut out,
        )
    };
    if ok == 0 {
        return None;
    }
    let slice = unsafe { std::slice::from_raw_parts(out.pb_data, out.cb_data as usize) };
    let v = slice.to_vec();
    unsafe { LocalFree(out.pb_data as *mut c_void) };
    Some(v)
}

// keyring（非 Windows：macOS Keychain / Linux Secret Service）

/// 非 Windows 存储引用前缀（JSON 中只存该标记，不存密文）
#[cfg(not(windows))]
const KEYRING_PREFIX: &str = "keyring:";

#[cfg(not(windows))]
mod keyring_store {
    use keyring::Entry;

    const SERVICE: &str = "RustShellV3";

    fn entry(key: &str) -> Result<Entry, keyring::Error> {
        Entry::new(SERVICE, key)
    }

    pub fn set(key: &str, plain: &str) -> Result<(), keyring::Error> {
        entry(key)?.set_password(plain)
    }

    pub fn get(key: &str) -> Option<String> {
        entry(key).ok()?.get_password().ok()
    }
}

// 公开 API

/// 加密并编码为可持久化字符串。
pub fn protect(key: &str, plain: &str) -> Result<String> {
    if plain.is_empty() {
        anyhow::bail!("empty password");
    }
    #[cfg(windows)]
    {
        let _ = key;
        dpapi_protect(plain)
            .map(|v| b64_encode(&v))
            .ok_or_else(|| anyhow::anyhow!("DPAPI protect failed"))
    }
    #[cfg(not(windows))]
    {
        keyring_store::set(key, plain).map_err(|e| anyhow::anyhow!("keyring set failed: {e}"))?;
        Ok(format!("{KEYRING_PREFIX}{key}"))
    }
}

/// 解码并解密（与 protect 对应）。
pub fn unprotect(stored: &str) -> Option<String> {
    #[cfg(windows)]
    {
        dpapi_unprotect(stored).and_then(|v| String::from_utf8(v).ok())
    }
    #[cfg(not(windows))]
    {
        if let Some(key) = stored.strip_prefix(KEYRING_PREFIX) {
            return keyring_store::get(key);
        }
        // 遗留 Base64：兼容读取，下次保存升级为 keyring 引用
        b64_decode(stored).and_then(|v| String::from_utf8(v).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        for s in ["", "a", "ab", "abc", "hello world", "p@ss w0rd!中文"] {
            assert_eq!(b64_decode(&b64_encode(s.as_bytes())).unwrap(), s.as_bytes());
        }
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_round_trip() {
        let stored = protect("test-entry", "secret-pass").unwrap();
        assert_ne!(stored, "secret-pass");
        assert_eq!(unprotect(&stored).unwrap(), "secret-pass");
    }
}
