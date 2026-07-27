#[cfg(all(not(windows), test))]
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const STORE_FILE: &str = "github_token_store.json";

#[derive(Debug, Error)]
pub enum GitHubTokenStoreError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid token store")]
    InvalidStore,
    #[error("secure token storage is not available on this platform")]
    SecureStorageUnavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitHubStoredAccount {
    pub login: String,
    pub id: i64,
    pub avatar_url: Option<String>,
    pub html_url: Option<String>,
    pub scopes: Vec<String>,
    pub connected_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct GitHubStoredToken {
    pub access_token: SecretString,
    pub scopes: Vec<String>,
    pub account: Option<GitHubStoredAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenStoreFile {
    version: u8,
    ciphertext: String,
    scopes: Vec<String>,
    account: Option<GitHubStoredAccount>,
}

#[derive(Debug, Clone)]
pub struct LocalEncryptedGitHubTokenStore {
    path: PathBuf,
    legacy_path: Option<PathBuf>,
}

impl LocalEncryptedGitHubTokenStore {
    pub fn new_default() -> Result<Self, GitHubTokenStoreError> {
        let path = default_store_path()?;
        let legacy_path = legacy_store_path().filter(|legacy_path| legacy_path != &path);
        Ok(Self { path, legacy_path })
    }

    pub fn new_for_path(path: PathBuf) -> Self {
        Self {
            path,
            legacy_path: None,
        }
    }

    pub fn store(&self, token: GitHubStoredToken) -> Result<(), GitHubTokenStoreError> {
        Self::store_at_path(&self.path, token)?;
        self.clear_legacy_store_best_effort();
        Ok(())
    }

    fn store_at_path(path: &Path, token: GitHubStoredToken) -> Result<(), GitHubTokenStoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = TokenStoreFile {
            version: 1,
            ciphertext: STANDARD.encode(protect_token(
                path,
                token.access_token.expose_secret().as_bytes(),
            )?),
            scopes: token.scopes,
            account: token.account,
        };
        let body =
            serde_json::to_vec_pretty(&file).map_err(|_| GitHubTokenStoreError::InvalidStore)?;
        fs::write(path, body)?;
        Ok(())
    }

    pub fn load(&self) -> Result<Option<GitHubStoredToken>, GitHubTokenStoreError> {
        if self.path.exists() {
            return Self::load_from_path(&self.path);
        }

        let Some(legacy_path) = self
            .legacy_path
            .as_deref()
            .filter(|legacy_path| legacy_path.exists())
        else {
            return Ok(None);
        };
        let Some(token) = Self::load_from_path(legacy_path)? else {
            return Ok(None);
        };

        Self::store_at_path(&self.path, token.clone())?;
        self.clear_legacy_store_best_effort();
        Ok(Some(token))
    }

    fn load_from_path(path: &Path) -> Result<Option<GitHubStoredToken>, GitHubTokenStoreError> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read(path)?;
        let file: TokenStoreFile =
            serde_json::from_slice(&raw).map_err(|_| GitHubTokenStoreError::InvalidStore)?;
        let encrypted = STANDARD
            .decode(file.ciphertext)
            .map_err(|_| GitHubTokenStoreError::InvalidStore)?;
        let decrypted = unprotect_token(path, &encrypted)?;
        let access_token =
            String::from_utf8(decrypted).map_err(|_| GitHubTokenStoreError::InvalidStore)?;

        Ok(Some(GitHubStoredToken {
            access_token: SecretString::from(access_token),
            scopes: file.scopes,
            account: file.account,
        }))
    }

    pub fn clear(&self) -> Result<(), GitHubTokenStoreError> {
        let primary_result = clear_store_path(&self.path);
        let legacy_result = self
            .legacy_path
            .as_deref()
            .map(clear_store_path)
            .unwrap_or(Ok(()));

        primary_result.and(legacy_result)
    }

    fn clear_legacy_store_best_effort(&self) {
        let Some(legacy_path) = self
            .legacy_path
            .as_deref()
            .filter(|legacy_path| legacy_path.exists())
        else {
            return;
        };

        if let Err(error) = clear_store_path(legacy_path) {
            tracing::warn!(
                path = %legacy_path.display(),
                %error,
                "failed to remove legacy GitHub token store after migration"
            );
        }
    }
}

fn default_store_path() -> Result<PathBuf, GitHubTokenStoreError> {
    directories::ProjectDirs::from("ai", "openteams-lab", "openteams")
        .map(|dirs| dirs.data_dir().join(STORE_FILE))
        .ok_or(GitHubTokenStoreError::SecureStorageUnavailable)
}

fn legacy_store_path() -> Option<PathBuf> {
    dirs::config_dir().map(|base| base.join("openteams").join(STORE_FILE))
}

fn clear_store_path(path: &Path) -> Result<(), GitHubTokenStoreError> {
    let file_result = match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    };
    let secure_token_result = clear_secure_token(path);

    file_result.and(secure_token_result)
}

#[cfg(windows)]
fn protect_token(_store_path: &Path, input: &[u8]) -> Result<Vec<u8>, GitHubTokenStoreError> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData},
    };

    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &in_blob,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err(GitHubTokenStoreError::InvalidStore);
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec();
    unsafe {
        LocalFree(out_blob.pbData.cast());
    }
    Ok(bytes)
}

#[cfg(windows)]
fn unprotect_token(_store_path: &Path, input: &[u8]) -> Result<Vec<u8>, GitHubTokenStoreError> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
        },
    };

    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &in_blob,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err(GitHubTokenStoreError::InvalidStore);
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec();
    unsafe {
        LocalFree(out_blob.pbData.cast());
    }
    Ok(bytes)
}

#[cfg(all(not(windows), not(test)))]
fn protect_token(store_path: &Path, input: &[u8]) -> Result<Vec<u8>, GitHubTokenStoreError> {
    const KEYRING_MARKER: &[u8] = b"openteams-github-keyring:v1";

    let token = std::str::from_utf8(input).map_err(|_| GitHubTokenStoreError::InvalidStore)?;
    keyring_entry(store_path)?
        .set_password(token)
        .map_err(map_keyring_error)?;
    Ok(KEYRING_MARKER.to_vec())
}

#[cfg(all(not(windows), not(test)))]
fn unprotect_token(store_path: &Path, input: &[u8]) -> Result<Vec<u8>, GitHubTokenStoreError> {
    const KEYRING_MARKER: &[u8] = b"openteams-github-keyring:v1";

    if input != KEYRING_MARKER {
        return Err(GitHubTokenStoreError::InvalidStore);
    }
    let token = keyring_entry(store_path)?
        .get_password()
        .map_err(map_keyring_error)?;
    Ok(token.into_bytes())
}

#[cfg(windows)]
fn clear_secure_token(_store_path: &Path) -> Result<(), GitHubTokenStoreError> {
    Ok(())
}

#[cfg(all(not(windows), not(test)))]
fn clear_secure_token(store_path: &Path) -> Result<(), GitHubTokenStoreError> {
    let entry = keyring_entry(store_path)?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(err) => Err(map_keyring_error(err)),
    }
}

#[cfg(all(not(windows), not(test)))]
fn keyring_entry(store_path: &Path) -> Result<keyring::Entry, GitHubTokenStoreError> {
    const KEYRING_SERVICE: &str = "openteams.github";

    keyring::Entry::new(KEYRING_SERVICE, &keyring_account(store_path)).map_err(map_keyring_error)
}

#[cfg(all(not(windows), not(test)))]
fn keyring_account(store_path: &Path) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(store_path.to_string_lossy().as_bytes());
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("github-device-flow:{hex}")
}

#[cfg(all(not(windows), not(test)))]
fn map_keyring_error(err: keyring::Error) -> GitHubTokenStoreError {
    GitHubTokenStoreError::Io(io::Error::other(format!(
        "secure token storage failed: {err}"
    )))
}

#[cfg(all(not(windows), test))]
fn test_secure_tokens() -> &'static Mutex<HashMap<PathBuf, Vec<u8>>> {
    static TOKENS: OnceLock<Mutex<HashMap<PathBuf, Vec<u8>>>> = OnceLock::new();
    TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(all(not(windows), test))]
fn protect_token(store_path: &Path, input: &[u8]) -> Result<Vec<u8>, GitHubTokenStoreError> {
    const KEYRING_MARKER: &[u8] = b"openteams-github-keyring:v1";

    test_secure_tokens()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(store_path.to_path_buf(), input.to_vec());
    Ok(KEYRING_MARKER.to_vec())
}

#[cfg(all(not(windows), test))]
fn unprotect_token(store_path: &Path, input: &[u8]) -> Result<Vec<u8>, GitHubTokenStoreError> {
    const KEYRING_MARKER: &[u8] = b"openteams-github-keyring:v1";

    if input != KEYRING_MARKER {
        return Err(GitHubTokenStoreError::InvalidStore);
    }
    test_secure_tokens()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(store_path)
        .cloned()
        .ok_or(GitHubTokenStoreError::InvalidStore)
}

#[cfg(all(not(windows), test))]
fn clear_secure_token(store_path: &Path) -> Result<(), GitHubTokenStoreError> {
    test_secure_tokens()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(store_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use secrecy::ExposeSecret;
    use tempfile::tempdir;

    use super::{
        GitHubStoredAccount, GitHubStoredToken, LocalEncryptedGitHubTokenStore, STORE_FILE,
        default_store_path,
    };

    fn stored_token(token: &str) -> GitHubStoredToken {
        GitHubStoredToken {
            access_token: token.to_string().into(),
            scopes: vec!["repo".to_string()],
            account: Some(GitHubStoredAccount {
                login: "octo".to_string(),
                id: 1,
                avatar_url: None,
                html_url: None,
                scopes: vec!["repo".to_string()],
                connected_at: Utc::now(),
            }),
        }
    }

    #[test]
    fn default_store_uses_shared_application_data_directory() {
        let expected = directories::ProjectDirs::from("ai", "openteams-lab", "openteams")
            .expect("project directory")
            .data_dir()
            .join(STORE_FILE);

        assert_eq!(default_store_path().expect("default store path"), expected);
    }

    #[test]
    fn token_store_roundtrips_without_plaintext_file_content() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("token.json");
        let store = LocalEncryptedGitHubTokenStore::new_for_path(path.clone());
        let token = "gho_secret_token_value";

        store.store(stored_token(token)).expect("store token");

        let raw = std::fs::read_to_string(&path).expect("read token file");
        assert!(!raw.contains(token));
        let loaded = store.load().expect("load token").expect("token exists");
        assert_eq!(loaded.access_token.expose_secret(), token);
    }

    #[test]
    fn legacy_token_store_migrates_on_load() {
        let dir = tempdir().expect("tempdir");
        let legacy_path = dir.path().join("legacy").join(STORE_FILE);
        let current_path = dir.path().join("current").join(STORE_FILE);
        let token = "gho_legacy_secret_token_value";
        let legacy_store = LocalEncryptedGitHubTokenStore::new_for_path(legacy_path.clone());
        legacy_store
            .store(stored_token(token))
            .expect("store legacy token");

        let store = LocalEncryptedGitHubTokenStore {
            path: current_path.clone(),
            legacy_path: Some(legacy_path.clone()),
        };
        let loaded = store.load().expect("migrate token").expect("token exists");

        assert_eq!(loaded.access_token.expose_secret(), token);
        assert!(current_path.exists());
        assert!(!legacy_path.exists());
        let reloaded = store.load().expect("reload token").expect("token exists");
        assert_eq!(reloaded.access_token.expose_secret(), token);
    }
}
