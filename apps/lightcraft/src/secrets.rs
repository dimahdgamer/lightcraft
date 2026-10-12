//! The Nextcloud app password in the system's credential store, so it survives a restart.
//!
//! Windows only for now (the Credential Manager, through `keyring-core` and
//! `windows-native-keyring-store`: no C code, no `unsafe` of ours). Elsewhere the engine keeps the
//! password in memory for the session and the Nextcloud dialog says so.
//!
//! A credential is stored as a generic credential named `<user>@<server address>.LightCraft
//! Nextcloud`, readable by the same Windows user only.

use std::sync::Arc;

use keyring_core::Entry;
use lightcraft_engine::nextcloud::SecretStore;

/// The service name the credentials are filed under.
const SERVICE: &str = "LightCraft Nextcloud";

pub struct SystemSecrets;

impl SystemSecrets {
    /// Make the Credential Manager the default store; `None` (logged) if it can't be reached.
    pub fn install() -> Option<Arc<dyn SecretStore>> {
        match windows_native_keyring_store::Store::new() {
            Ok(store) => {
                keyring_core::set_default_store(store);
                Some(Arc::new(SystemSecrets))
            }
            Err(e) => {
                log::warn!("the credential store is not available, so the Nextcloud app password is not remembered: {e}");
                None
            }
        }
    }
}

fn entry(key: &str) -> Result<Entry, String> {
    Entry::new(SERVICE, key).map_err(|e| e.to_string())
}

impl SecretStore for SystemSecrets {
    fn get(&self, key: &str) -> Option<String> {
        entry(key).ok()?.get_password().ok()
    }

    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        entry(key)?.set_password(secret).map_err(|e| e.to_string())
    }

    fn delete(&self, key: &str) -> Result<(), String> {
        match entry(key)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real Credential Manager: write, read, replace, delete, and a missing credential.
    #[test]
    fn a_secret_goes_into_the_credential_manager_and_comes_back() {
        let store = SystemSecrets::install().expect("the Credential Manager is reachable on Windows");
        let key = format!("lightcraft-test-{}@http://127.0.0.1:1", std::process::id());
        assert_eq!(store.get(&key), None, "nothing before it is set");
        store.set(&key, "app-pw-1").unwrap();
        assert_eq!(store.get(&key).as_deref(), Some("app-pw-1"));
        store.set(&key, "app-pw-2 with ünïcode and : colons").unwrap();
        assert_eq!(store.get(&key).as_deref(), Some("app-pw-2 with ünïcode and : colons"), "a new password replaces the old");
        store.delete(&key).unwrap();
        assert_eq!(store.get(&key), None, "gone after delete");
        store.delete(&key).unwrap();
    }
}
