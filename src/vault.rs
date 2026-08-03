//! The in-memory vault: every repo file lives in a BTreeMap, never on disk.
//! `.enc` entries are decrypted in place with the same scheme as ed.sh:
//! `openssl enc -aes-256-cbc -pbkdf2 -salt` (PBKDF2-HMAC-SHA256, 10 000
//! iterations, "Salted__" header, key 32 B + IV 16 B, PKCS#7 padding).

use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::io::Read;
use zeroize::Zeroizing;

type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

const OPENSSL_MAGIC: &[u8; 8] = b"Salted__";
const PBKDF2_ITERS: u32 = 10_000;

#[derive(Default)]
pub struct MemFs {
    /// normalized repo-relative path -> file bytes
    pub files: BTreeMap<String, Vec<u8>>,
}

impl MemFs {
    /// Load from an in-memory zip archive (Azure DevOps `$format=zip`).
    /// Strips a single shared top-level directory if the archive has one.
    pub fn from_zip(bytes: &[u8]) -> Result<Self> {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .context("response is not a valid zip archive")?;
        let mut fs = MemFs::default();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            if !entry.is_file() {
                continue;
            }
            let name = entry.name().replace('\\', "/");
            let mut data = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut data)?;
            fs.files.insert(name, data);
        }
        fs.strip_common_root();
        Ok(fs)
    }

    /// Load from a local directory (config `localPath`, used for testing).
    pub fn from_dir(root: &str) -> Result<Self> {
        let mut fs = MemFs::default();
        let base = std::path::Path::new(root);
        ensure!(base.is_dir(), "localPath '{root}' is not a directory");
        let mut stack = vec![base.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let path = entry?.path();
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name == ".git" || name == ".idea" {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                } else if path.is_file() {
                    let rel = path
                        .strip_prefix(base)?
                        .to_string_lossy()
                        .replace('\\', "/");
                    fs.files.insert(rel, std::fs::read(&path)?);
                }
            }
        }
        Ok(fs)
    }

    fn strip_common_root(&mut self) {
        let Some(first) = self.files.keys().next() else {
            return;
        };
        let Some(root) = first.split('/').next().map(str::to_owned) else {
            return;
        };
        let prefix = format!("{root}/");
        if !self.files.keys().all(|k| k.starts_with(&prefix)) {
            return;
        }
        self.files = std::mem::take(&mut self.files)
            .into_iter()
            .map(|(k, v)| (k[prefix.len()..].to_string(), v))
            .collect();
    }

    /// Decrypt every `.enc` file in place (the plaintext entry replaces it,
    /// `.enc`/`.enc.mode` entries are removed). Fails fast on the first file
    /// so a wrong key is caught immediately.
    pub fn decrypt_all(&mut self, passphrase: &Zeroizing<String>) -> Result<usize> {
        let enc_paths: Vec<String> = self
            .files
            .keys()
            .filter(|k| k.ends_with(".enc"))
            .cloned()
            .collect();
        let mut count = 0;
        for path in &enc_paths {
            // decrypt BEFORE removing, so a wrong key leaves the vault
            // intact for the retry
            let data = self.files.get(path).unwrap();
            let plain = decrypt_openssl(data, passphrase.as_bytes())
                .with_context(|| format!("decrypting '{path}' (wrong key?)"))?;
            let target = path.trim_end_matches(".enc").to_string();
            self.files.remove(path);
            self.files.insert(target, plain);
            self.files.remove(&format!("{path}.mode"));
            count += 1;
        }
        // leftover .mode files whose .enc was already gone
        self.files.retain(|k, _| !k.ends_with(".enc.mode"));
        Ok(count)
    }

    pub fn get(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(|v| v.as_slice())
    }

    /// Immediate subdirectories of `prefix` ("" = repo root), sorted.
    pub fn subdirs(&self, prefix: &str) -> Vec<String> {
        let pfx = if prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", prefix.trim_end_matches('/'))
        };
        let mut out: Vec<String> = self
            .files
            .keys()
            .filter_map(|k| k.strip_prefix(&pfx))
            .filter_map(|rest| {
                let mut parts = rest.splitn(2, '/');
                let first = parts.next()?;
                parts.next()?; // only paths that go deeper => `first` is a dir
                Some(first.to_string())
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// True when the bytes carry the OpenSSL `Salted__` encryption header.
pub fn looks_encrypted(data: &[u8]) -> bool {
    data.len() > 16 && &data[..8] == OPENSSL_MAGIC
}

/// OpenSSL `enc -aes-256-cbc -pbkdf2 -salt` compatible decryption.
pub fn decrypt_openssl(data: &[u8], pass: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        data.len() > 16 && &data[..8] == OPENSSL_MAGIC,
        "missing OpenSSL 'Salted__' header"
    );
    let salt = &data[8..16];
    let mut key_iv = Zeroizing::new([0u8; 48]);
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(pass, salt, PBKDF2_ITERS, key_iv.as_mut());
    let (key, iv) = key_iv.split_at(32);

    let ciphertext = &data[16..];
    if !ciphertext.len().is_multiple_of(16) {
        bail!("ciphertext length is not a multiple of the AES block size");
    }
    Aes256CbcDec::new_from_slices(key, iv)
        .expect("key/iv sizes are fixed")
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|_| anyhow::anyhow!("bad padding — wrong passphrase or corrupted data"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixture generated with:
    //   printf 'hello hefesto\n' | openssl enc -aes-256-cbc -pbkdf2 -salt -pass pass:forge
    // (see tests/make_fixture.sh)
    #[test]
    fn decrypts_openssl_output() {
        let enc = include_bytes!("../tests/fixture.enc");
        let plain = decrypt_openssl(enc, b"forge").expect("decrypt");
        assert_eq!(plain, b"hello hefesto\n");
    }

    #[test]
    fn wrong_key_fails() {
        let enc = include_bytes!("../tests/fixture.enc");
        assert!(decrypt_openssl(enc, b"not-the-key").is_err());
    }
}
