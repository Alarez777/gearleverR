//! Stores the GitHub token in the system keyring, with a machine-bound
//! encrypted fallback.
//!
//! Tier 1 is the freedesktop Secret Service (GNOME Keyring, `KWallet`,
//! `KeePassXC`) accessed through the blocking `secret-service` API. Tier 2 is an
//! AES-256-GCM blob kept in GSettings whose key is derived from the host
//! machine-id, the user id and the application id. Tier 2 is obfuscation, not
//! strong secrecy: it makes a copied configuration useless on another machine,
//! but a process running as the same user can derive the same key.
//!
//! Exactly one tier is preferred at a time: a successful keyring write clears
//! the blob, and a successful blob write tries to clear the keyring. Reads try
//! the keyring first and fall back to the blob when no unlocked item exists.

use std::collections::HashMap;
use std::fmt;
use std::fs;

use aes_gcm::aead::{Aead as _, KeyInit as _};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hkdf::Hkdf;
use secret_service::EncryptionType;
use secret_service::blocking::SecretService;
use sha2::Sha256;
use zbus::blocking::Connection;
use zbus::blocking::fdo::DBusProxy;
use zeroize::Zeroizing;

use crate::settings::{APP_ID, Settings};

const SECRETS_SERVICE: &str = "org.freedesktop.secrets";
const KEYRING_APPLICATION: &str = APP_ID;
const KEYRING_PURPOSE: &str = "github-token";
const KEYRING_LABEL: &str = "Gear LeverR GitHub token";

const BLOB_PREFIX: &str = "GLR1:";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const HKDF_INFO: &[u8] = b"github-token-v1";

/// Reads and writes the GitHub token.
#[derive(Clone)]
pub(crate) struct TokenStore {
    settings: Settings,
}

impl fmt::Debug for TokenStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenStore")
    }
}

impl TokenStore {
    /// Creates a store backed by the given settings snapshot.
    #[must_use]
    pub(crate) fn new(settings: Settings) -> Self {
        Self { settings }
    }

    /// Returns the stored token, trying the keyring before the encrypted blob.
    #[must_use]
    pub(crate) fn get(&self) -> Option<String> {
        match Self::get_keyring() {
            Ok(Some(token)) => return Some(token),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(%error, "cannot read the GitHub token from the keyring");
            }
        }

        self.get_cipher()
    }

    /// Stores a token, preferring the keyring and falling back to the blob.
    ///
    /// # Errors
    ///
    /// Returns an error when the token is empty or malformed, when the keyring
    /// is unavailable and the encrypted fallback cannot be written, or when
    /// the machine-id needed by the fallback cannot be read.
    pub(crate) fn set(&self, token: &str) -> Result<()> {
        let token = normalize_token(token)?;

        match Self::set_keyring(token) {
            Ok(()) => {
                let _ = self.settings.set_github_token_cipher("");
                Ok(())
            }
            Err(keyring_error) => {
                tracing::debug!(
                    %keyring_error,
                    "system keyring unavailable, using the encrypted fallback"
                );
                let blob = encrypt(token)?;
                if !self.settings.set_github_token_cipher(&blob) {
                    bail!("cannot store the encrypted GitHub token in GSettings");
                }
                if let Err(error) = Self::clear_keyring() {
                    tracing::debug!(%error, "cannot remove the old keyring token");
                }
                Ok(())
            }
        }
    }

    /// Removes the token from both tiers.
    ///
    /// # Errors
    ///
    /// Returns an error when the encrypted blob exists but cannot be cleared,
    /// or when the keyring is running and refuses to delete the item.
    pub(crate) fn clear(&self) -> Result<()> {
        let cipher_cleared = self.settings.set_github_token_cipher("");
        let keyring_result = Self::clear_keyring();

        if !cipher_cleared && self.settings.github_token_cipher().is_some() {
            bail!("cannot clear the encrypted GitHub token from GSettings");
        }

        keyring_result.context("clearing the GitHub token from the keyring")?;
        Ok(())
    }

    fn get_keyring() -> Result<Option<String>> {
        let Some(connection) = keyring_connection() else {
            return Ok(None);
        };
        let service = SecretService::connect_with_existing(EncryptionType::Dh, connection)
            .context("connecting to the Secret Service")?;
        let items = service
            .search_items(keyring_attributes())
            .context("searching the keyring for the GitHub token")?;

        let Some(item) = items.unlocked.first() else {
            if !items.locked.is_empty() {
                tracing::debug!("the GitHub token keyring item is locked");
            }
            return Ok(None);
        };

        let secret = item
            .get_secret()
            .context("reading the GitHub token from the keyring")?;
        let token = String::from_utf8(secret).context("the keyring token is not valid UTF-8")?;
        let token = token.trim();

        if token.is_empty() {
            return Ok(None);
        }
        if !valid_token(token) {
            bail!("the keyring token contains invalid characters");
        }

        Ok(Some(token.to_owned()))
    }

    fn set_keyring(token: &str) -> Result<()> {
        let Some(connection) = keyring_connection() else {
            bail!("the Secret Service is not running");
        };
        let service = SecretService::connect_with_existing(EncryptionType::Dh, connection)
            .context("connecting to the Secret Service")?;
        let collection = service
            .get_default_collection()
            .or_else(|_| service.get_any_collection())
            .context("opening the default keyring collection")?;
        collection
            .create_item(
                KEYRING_LABEL,
                keyring_attributes(),
                token.as_bytes(),
                true,
                "text/plain",
            )
            .context("storing the GitHub token in the keyring")?;

        Ok(())
    }

    fn clear_keyring() -> Result<()> {
        let Some(connection) = keyring_connection() else {
            return Ok(());
        };
        let service = SecretService::connect_with_existing(EncryptionType::Dh, connection)
            .context("connecting to the Secret Service")?;
        let items = service
            .search_items(keyring_attributes())
            .context("searching the keyring for the GitHub token")?;

        for item in items.unlocked.iter().chain(items.locked.iter()) {
            if item.is_locked().context("checking the keyring item")? {
                item.unlock().context("unlocking the keyring item")?;
            }
            item.delete().context("deleting the keyring item")?;
        }

        Ok(())
    }

    fn get_cipher(&self) -> Option<String> {
        let blob = self.settings.github_token_cipher()?;

        match decrypt(&blob) {
            Ok(token) => {
                let token = token.trim();
                if valid_token(token) {
                    Some(token.to_owned())
                } else {
                    tracing::warn!("ignoring an invalid GitHub token in GSettings");
                    None
                }
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "cannot decrypt the GitHub token; it may belong to another machine or user"
                );
                None
            }
        }
    }
}

/// Returns whether `token` is a non-empty printable ASCII token.
#[must_use]
pub(crate) fn valid_token(token: &str) -> bool {
    let token = token.trim();
    !token.is_empty() && token.chars().all(|character| character.is_ascii_graphic())
}

fn normalize_token(token: &str) -> Result<&str> {
    let token = token.trim();
    if token.is_empty() {
        bail!("the token is empty");
    }
    if !valid_token(token) {
        bail!("the token contains invalid characters");
    }
    Ok(token)
}

fn keyring_attributes() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("application", KEYRING_APPLICATION),
        ("purpose", KEYRING_PURPOSE),
    ])
}

/// Opens the session bus and returns a connection only when the Secret Service
/// owns its well-known name.
///
/// Checking the owner avoids D-Bus activating a keyring daemon just to read a
/// token, which could otherwise block a background update check.
fn keyring_connection() -> Option<Connection> {
    let connection = match Connection::session() {
        Ok(connection) => connection,
        Err(error) => {
            tracing::debug!(%error, "session bus unavailable");
            return None;
        }
    };
    let proxy = match DBusProxy::new(&connection) {
        Ok(proxy) => proxy,
        Err(error) => {
            tracing::debug!(%error, "cannot query the session bus");
            return None;
        }
    };
    let Ok(name) = zbus::names::BusName::try_from(SECRETS_SERVICE) else {
        return None;
    };

    match proxy.name_has_owner(name) {
        Ok(true) => Some(connection),
        Ok(false) => {
            tracing::debug!("the Secret Service is not running");
            None
        }
        Err(error) => {
            tracing::debug!(%error, "cannot query the Secret Service owner");
            None
        }
    }
}

fn machine_id() -> Result<String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(contents) = fs::read_to_string(path) {
            let id = contents.trim();
            if !id.is_empty() {
                return Ok(id.to_owned());
            }
        }
    }

    bail!("cannot read the machine-id needed by the encrypted token fallback")
}

fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

fn derive_key(machine_id: &str, uid: u32, salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    let input = format!("{machine_id}:{uid}:{APP_ID}");
    let hkdf = Hkdf::<Sha256>::new(Some(salt), input.as_bytes());
    let mut key = [0_u8; KEY_LEN];
    hkdf.expand(HKDF_INFO, &mut key)
        .map_err(|_| anyhow!("cannot derive the token encryption key"))?;

    Ok(key)
}

fn encrypt(token: &str) -> Result<String> {
    let machine_id = machine_id()?;
    encrypt_with(token, &machine_id, current_uid())
}

fn encrypt_with(token: &str, machine_id: &str, uid: u32) -> Result<String> {
    let mut salt = [0_u8; SALT_LEN];
    getrandom::fill(&mut salt)
        .map_err(|error| anyhow!("cannot generate a token encryption salt: {error}"))?;
    let mut nonce = [0_u8; NONCE_LEN];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow!("cannot generate a token encryption nonce: {error}"))?;

    let key = Zeroizing::new(derive_key(machine_id, uid, &salt)?);
    let cipher = Aes256Gcm::new_from_slice(&key[..])
        .map_err(|_| anyhow!("cannot initialize AES-256-GCM"))?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), token.as_bytes())
        .map_err(|_| anyhow!("cannot encrypt the GitHub token"))?;

    let mut payload = Vec::with_capacity(SALT_LEN + NONCE_LEN + ciphertext.len());
    payload.extend_from_slice(&salt);
    payload.extend_from_slice(&nonce);
    payload.extend_from_slice(&ciphertext);

    Ok(format!("{BLOB_PREFIX}{}", STANDARD.encode(payload)))
}

fn decrypt(blob: &str) -> Result<String> {
    let machine_id = machine_id()?;
    decrypt_with(blob, &machine_id, current_uid())
}

fn decrypt_with(blob: &str, machine_id: &str, uid: u32) -> Result<String> {
    let encoded = blob
        .strip_prefix(BLOB_PREFIX)
        .ok_or_else(|| anyhow!("unknown GitHub token blob format"))?;
    let payload = STANDARD
        .decode(encoded)
        .map_err(|error| anyhow!("invalid GitHub token blob: {error}"))?;
    if payload.len() < SALT_LEN + NONCE_LEN {
        bail!("truncated GitHub token blob");
    }

    let (salt, rest) = payload.split_at(SALT_LEN);
    let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
    let key = Zeroizing::new(derive_key(machine_id, uid, salt)?);
    let cipher = Aes256Gcm::new_from_slice(&key[..])
        .map_err(|_| anyhow!("cannot initialize AES-256-GCM"))?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| anyhow!("cannot decrypt the GitHub token for this machine and user"))?,
    );

    String::from_utf8(plaintext.to_vec())
        .map_err(|error| anyhow!("decrypted GitHub token is not UTF-8: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MACHINE_A: &str = "0123456789abcdef0123456789abcdef";
    const MACHINE_B: &str = "fedcba9876543210fedcba9876543210";
    const TOKEN: &str = "ghp_test_token_123";

    #[test]
    fn validates_printable_tokens() {
        assert!(valid_token(TOKEN));
        assert!(valid_token("  github_pat_abc_123  "));
        assert!(!valid_token(""));
        assert!(!valid_token("   "));
        assert!(!valid_token("token with spaces"));
        assert!(!valid_token("token\nwith\nnewlines"));
    }

    #[test]
    fn round_trips_the_token() {
        let blob = encrypt_with(TOKEN, MACHINE_A, 1000).unwrap();
        assert!(blob.starts_with(BLOB_PREFIX));
        assert_eq!(decrypt_with(&blob, MACHINE_A, 1000).unwrap(), TOKEN);
    }

    #[test]
    fn rejects_another_machine_or_user() {
        let blob = encrypt_with(TOKEN, MACHINE_A, 1000).unwrap();

        assert!(decrypt_with(&blob, MACHINE_B, 1000).is_err());
        assert!(decrypt_with(&blob, MACHINE_A, 1001).is_err());
    }

    #[test]
    fn rejects_tampering_and_unknown_formats() {
        let blob = encrypt_with(TOKEN, MACHINE_A, 1000).unwrap();
        let encoded = blob.strip_prefix(BLOB_PREFIX).unwrap();
        let mut payload = STANDARD.decode(encoded).unwrap();
        payload[SALT_LEN + NONCE_LEN] ^= 1;
        let tampered = format!("{BLOB_PREFIX}{}", STANDARD.encode(payload));

        assert!(decrypt_with(&tampered, MACHINE_A, 1000).is_err());
        assert!(decrypt_with("nope", MACHINE_A, 1000).is_err());
        assert!(decrypt_with(BLOB_PREFIX, MACHINE_A, 1000).is_err());
    }
}
