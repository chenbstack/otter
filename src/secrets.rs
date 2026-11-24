//! 秘密(密码、私钥、口令)的加密。
//!
//! 一把 32 字节随机主密钥放在系统凭据库(macOS 钥匙串 / Windows 凭据管理器 / Linux Secret Service),
//! 秘密用 XChaCha20-Poly1305 加密后存进 SQLite。每条密文绑定 AAD(`节点id:槽位`),
//! 拷到别的行就解不开。调试可用 `OTTER_MASTER_KEY_FILE` 改读本地文件。
//!
//! Linux 上没有 Secret Service(无桌面环境、没装 gnome-keyring/KWallet)时,回退到数据目录下的
//! `master.key`(只有所有者可读写)。这比系统凭据库弱——拿到你的用户权限就能读到——但仍然不是明文存密码。

use chacha20poly1305::aead::{Aead, Generate, Key, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

const SERVICE: &str = "otter";
const ACCOUNT: &str = "master-key";

pub struct Vault {
    key: Zeroizing<[u8; 32]>,
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn key_from_hex(s: &str) -> Result<Zeroizing<[u8; 32]>, String> {
    let bytes = from_hex(s).filter(|b| b.len() == 32).ok_or("主密钥格式不对")?;
    let mut k = [0u8; 32];
    k.copy_from_slice(&bytes);
    Ok(Zeroizing::new(k))
}

fn new_key() -> Zeroizing<[u8; 32]> {
    let k = Key::<XChaCha20Poly1305>::generate();
    let mut out = [0u8; 32];
    out.copy_from_slice(&k);
    Zeroizing::new(out)
}

impl Vault {
    /// 取主密钥;没有就生成并存起来。系统凭据库不可用时返回错误(不回退到明文)。
    pub fn open() -> Result<Self, String> {
        if let Some(path) = std::env::var_os("OTTER_MASTER_KEY_FILE") {
            return Self::open_file(std::path::Path::new(&path));
        }
        match Self::open_keyring() {
            Ok(v) => Ok(v),
            #[cfg(all(unix, not(target_os = "macos")))]
            Err(e) => {
                let path = crate::store::data_dir().ok_or("找不到数据目录")?.join("master.key");
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                crate::logs::push(crate::logs::Level::Warn, "密钥", format!("{e};改用密钥文件 {}", path.display()));
                Self::open_file(&path)
            }
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            Err(e) => Err(e),
        }
    }

    fn open_keyring() -> Result<Self, String> {
        let unavailable = |e: &dyn std::fmt::Display| format!("系统凭据库不可用:{e}。无法保存或读取密码、私钥");
        let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(|e| unavailable(&e))?;
        match entry.get_password() {
            Ok(s) => Ok(Self { key: key_from_hex(&s)? }),
            Err(keyring::Error::NoEntry) => {
                let key = new_key();
                entry.set_password(&to_hex(&*key)).map_err(|e| unavailable(&e))?;
                Ok(Self { key })
            }
            Err(e) => Err(unavailable(&e)),
        }
    }

    /// 调试用:主密钥放本地文件。创建时只有所有者可读写,读取时拒绝别人也能读的文件。
    fn open_file(path: &std::path::Path) -> Result<Self, String> {
        use std::io::Write;
        match std::fs::read_to_string(path) {
            Ok(s) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = std::fs::metadata(path).map_err(|e| format!("读取主密钥文件信息失败:{e}"))?.permissions().mode();
                    if mode & 0o077 != 0 {
                        return Err(format!("主密钥文件权限过宽({:o}),请先 chmod 600 {}", mode & 0o777, path.display()));
                    }
                }
                Ok(Self { key: key_from_hex(&s)? })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = new_key();
                let mut opts = std::fs::OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.mode(0o600);
                }
                let mut f = opts.open(path).map_err(|e| format!("写入主密钥文件失败:{e}"))?;
                f.write_all(to_hex(&*key).as_bytes()).map_err(|e| format!("写入主密钥文件失败:{e}"))?;
                Ok(Self { key })
            }
            Err(e) => Err(format!("读取主密钥文件失败:{e}")),
        }
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self { key: new_key() }
    }

    /// 加密,返回 (nonce, 密文)。`aad` 用来把密文绑定到它所在的行。
    pub fn seal(&self, plain: &[u8], aad: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
        let cipher = XChaCha20Poly1305::new_from_slice(&*self.key).map_err(|_| "主密钥长度不对".to_string())?;
        let nonce = XNonce::generate();
        let ct = cipher.encrypt(&nonce, Payload { msg: plain, aad }).map_err(|_| "加密失败".to_string())?;
        Ok((nonce.to_vec(), ct))
    }

    /// 解密;密文被改过、`aad` 不对或密钥不对都会失败。
    pub fn open_secret(&self, nonce: &[u8], ct: &[u8], aad: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
        let nonce = XNonce::try_from(nonce).map_err(|_| "密文损坏(nonce 长度不对)".to_string())?;
        let cipher = XChaCha20Poly1305::new_from_slice(&*self.key).map_err(|_| "主密钥长度不对".to_string())?;
        cipher
            .decrypt(&nonce, Payload { msg: ct, aad })
            .map(Zeroizing::new)
            .map_err(|_| "解密失败:密文被改动,或主密钥与加密时不同".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let v = Vault::for_test();
        let (n, ct) = v.seal(b"hunter2", b"1:password").unwrap();
        assert_ne!(ct, b"hunter2");
        assert_eq!(&**v.open_secret(&n, &ct, b"1:password").unwrap(), b"hunter2");
    }

    #[test]
    fn tampered_or_moved_fails() {
        let v = Vault::for_test();
        let (n, mut ct) = v.seal(b"secret", b"1:password").unwrap();
        assert!(v.open_secret(&n, &ct, b"2:password").is_err(), "换了行(aad 不同)必须失败");
        ct[0] ^= 1;
        assert!(v.open_secret(&n, &ct, b"1:password").is_err(), "密文被改必须失败");
        assert!(v.open_secret(&n[..5], &ct, b"1:password").is_err());
    }

    #[test]
    fn other_key_fails() {
        let (n, ct) = Vault::for_test().seal(b"x", b"a").unwrap();
        assert!(Vault::for_test().open_secret(&n, &ct, b"a").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_owner_only_and_loose_perms_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("otter-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("k");
        let _ = std::fs::remove_file(&f);
        Vault::open_file(&f).unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        Vault::open_file(&f).unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Vault::open_file(&f).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_roundtrip() {
        let k = new_key();
        assert_eq!(*key_from_hex(&to_hex(&*k)).unwrap(), *k);
        assert!(key_from_hex("zz").is_err());
        assert!(key_from_hex("abcd").is_err());
    }
}
