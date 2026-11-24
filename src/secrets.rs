//! Secrets (alert topics/webhooks, Ring and Blink tokens) never touch the config
//! file or SQLite.
//!
//! * Windows: Windows Credential Manager through the `keyring` crate.
//! * Linux: a small encrypted file (ChaCha20-Poly1305). Its key comes from a
//!   systemd credential (`LoadCredentialEncrypted=umbra-key`, sealed to the TPM or
//!   the host key by `systemd-creds`), so a copied disk image or backup of the data
//!   directory can't be decrypted. Secret Service needs a desktop login session,
//!   which a headless server doesn't have, so it isn't used. Without the systemd
//!   credential (dev runs) the key falls back to a 0600 file next to the data.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAGIC: &[u8] = b"UMBRA1";

pub struct Secrets {
    backend: Backend,
    pub description: String,
}

enum Backend {
    #[cfg(windows)]
    Keyring,
    Sealed { path: PathBuf, key: [u8; 32], cache: Mutex<BTreeMap<String, String>> },
}

impl Secrets {
    pub fn open(data_dir: &Path) -> Result<Secrets, String> {
        #[cfg(windows)]
        {
            let _ = data_dir;
            Ok(Secrets { backend: Backend::Keyring, description: "Windows Credential Manager".into() })
        }
        #[cfg(not(windows))]
        {
            Self::open_sealed(data_dir)
        }
    }

    /// The encrypted-file store on any OS (the demo uses it so it never touches the OS keyring).
    pub fn open_file(data_dir: &Path) -> Result<Secrets, String> {
        Self::open_sealed(data_dir)
    }

    fn open_sealed(data_dir: &Path) -> Result<Secrets, String> {
        let (key, description) = match std::env::var_os("CREDENTIALS_DIRECTORY").map(|d| PathBuf::from(d).join("umbra-key")) {
            Some(p) if p.exists() => {
                let raw = std::fs::read(&p).map_err(|e| format!("reading systemd credential: {e}"))?;
                (derive(&raw), "encrypted file, key sealed by systemd-creds".to_string())
            }
            _ => {
                let kp = data_dir.join("secret.key");
                if !kp.exists() {
                    write_private(&kp, &crate::util::random::<32>())?;
                }
                let raw = std::fs::read(&kp).map_err(|e| format!("reading {}: {e}", kp.display()))?;
                crate::warn!("secrets: no systemd credential found, using {} (file permissions only)", kp.display());
                (derive(&raw), "encrypted file, key file in data dir (dev mode)".to_string())
            }
        };
        let path = data_dir.join("secrets.sealed");
        let cache = if path.exists() { load_sealed(&path, &key)? } else { BTreeMap::new() };
        Ok(Secrets { backend: Backend::Sealed { path, key, cache: Mutex::new(cache) }, description })
    }

    pub fn get(&self, name: &str) -> Option<String> {
        match &self.backend {
            #[cfg(windows)]
            Backend::Keyring => keyring::Entry::new("umbra", name).ok()?.get_password().ok(),
            Backend::Sealed { cache, .. } => cache.lock().unwrap_or_else(|e| e.into_inner()).get(name).cloned(),
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.get(name).is_some_and(|v| !v.is_empty())
    }

    pub fn set(&self, name: &str, value: &str) -> Result<(), String> {
        match &self.backend {
            #[cfg(windows)]
            Backend::Keyring => keyring::Entry::new("umbra", name).and_then(|e| e.set_password(value)).map_err(|e| e.to_string()),
            Backend::Sealed { path, key, cache } => {
                let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
                c.insert(name.to_string(), value.to_string());
                store_sealed(path, key, &c)
            }
        }
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        match &self.backend {
            #[cfg(windows)]
            Backend::Keyring => match keyring::Entry::new("umbra", name).and_then(|e| e.delete_credential()) {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e.to_string()),
            },
            Backend::Sealed { path, key, cache } => {
                let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
                c.remove(name);
                store_sealed(path, key, &c)
            }
        }
    }
}

fn derive(raw: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"umbra-secrets-v1");
    h.update(raw);
    h.finalize().into()
}

fn load_sealed(path: &Path, key: &[u8; 32]) -> Result<BTreeMap<String, String>, String> {
    let blob = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    if blob.len() < MAGIC.len() + 12 || &blob[..MAGIC.len()] != MAGIC {
        return Err(format!("{} is not an Umbra secrets file", path.display()));
    }
    let nonce = Nonce::from_slice(&blob[MAGIC.len()..MAGIC.len() + 12]);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let plain = cipher
        .decrypt(nonce, &blob[MAGIC.len() + 12..])
        .map_err(|_| format!("can't decrypt {} (the key changed?). Remove it and reconnect integrations.", path.display()))?;
    serde_json::from_slice(&plain).map_err(|e| e.to_string())
}

fn store_sealed(path: &Path, key: &[u8; 32], map: &BTreeMap<String, String>) -> Result<(), String> {
    let plain = serde_json::to_vec(map).map_err(|e| e.to_string())?;
    let nonce_bytes = crate::util::random::<12>();
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let ct = cipher.encrypt(Nonce::from_slice(&nonce_bytes), plain.as_ref()).map_err(|_| "encryption failed".to_string())?;
    let mut blob = MAGIC.to_vec();
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ct);
    write_private(path, &blob)
}

fn write_private(path: &Path, data: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        f.write_all(data).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, data).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("replacing {}: {e}", path.display()))
}
