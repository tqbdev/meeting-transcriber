//! The Soniox API key in the OS credential store: the Keychain on macOS,
//! Credential Manager on Windows.

use anyhow::Result;

const SERVICE: &str = "dev.tqbdev.meeting-transcriber";
const ACCOUNT: &str = "soniox-api-key";

/// Where the key is saved, for button labels.
pub const STORE_NAME: &str = if cfg!(target_os = "macos") {
    "Keychain"
} else if cfg!(windows) {
    "Credential Manager"
} else {
    "secure storage"
};

#[cfg(target_os = "macos")]
mod store {
    use anyhow::{Context, Result};
    use security_framework::passwords;

    /// errSecItemNotFound
    const NOT_FOUND: i32 = -25300;

    pub fn get(service: &str, account: &str) -> Option<String> {
        let bytes = passwords::get_generic_password(service, account).ok()?;
        String::from_utf8(bytes).ok()
    }

    pub fn set(service: &str, account: &str, secret: &str) -> Result<()> {
        passwords::set_generic_password(service, account, secret.as_bytes())
            .context("saving the key to the Keychain")
    }

    pub fn delete(service: &str, account: &str) -> Result<()> {
        match passwords::delete_generic_password(service, account) {
            Err(e) if e.code() != NOT_FOUND => Err(e).context("removing the key from the Keychain"),
            _ => Ok(()),
        }
    }
}

#[cfg(windows)]
mod store {
    use anyhow::{Context, Result};
    use keyring::{Entry, Error};

    pub fn get(service: &str, account: &str) -> Option<String> {
        Entry::new(service, account).ok()?.get_password().ok()
    }

    pub fn set(service: &str, account: &str, secret: &str) -> Result<()> {
        Entry::new(service, account)?
            .set_password(secret)
            .context("saving the key to Credential Manager")
    }

    pub fn delete(service: &str, account: &str) -> Result<()> {
        match Entry::new(service, account)?.delete_credential() {
            Err(Error::NoEntry) | Ok(()) => Ok(()),
            Err(e) => Err(e).context("removing the key from Credential Manager"),
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod store {
    use anyhow::{Result, bail};

    pub fn get(_: &str, _: &str) -> Option<String> {
        None
    }

    pub fn set(_: &str, _: &str, _: &str) -> Result<()> {
        bail!("saving the key isn't supported on this platform; set SONIOX_API_KEY instead")
    }

    pub fn delete(_: &str, _: &str) -> Result<()> {
        Ok(())
    }
}

pub fn load_api_key() -> Option<String> {
    store::get(SERVICE, ACCOUNT)
}

/// Saves the key, or removes the stored one when `key` is empty.
pub fn save_api_key(key: &str) -> Result<()> {
    if key.is_empty() {
        store::delete(SERVICE, ACCOUNT)
    } else {
        store::set(SERVICE, ACCOUNT, key)
    }
}
