//! Settings persistence (JSON file) and API-key storage (OS keychain).

use crate::models::Settings;
use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use std::fs;
use std::path::{Path, PathBuf};

const KEYRING_SERVICE: &str = "com.justinleahy.transcriptor";
pub const SECRET_OPENAI: &str = "openai_api_key";
pub const SECRET_ANTHROPIC: &str = "anthropic_api_key";

pub struct SettingsStore {
    path: PathBuf,
    current: RwLock<Settings>,
}

impl SettingsStore {
    pub fn load(config_dir: &Path) -> Result<Self> {
        fs::create_dir_all(config_dir)
            .with_context(|| format!("creating config dir {}", config_dir.display()))?;
        crate::fs_perms::harden_dir(config_dir);
        let path = config_dir.join("settings.json");
        let current = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Settings>(&text).unwrap_or_else(|e| {
                log::warn!("settings.json unreadable ({e}); using defaults");
                Settings::default()
            }),
            Err(_) => Settings::default(),
        };
        if path.exists() {
            crate::fs_perms::harden_file(&path);
        }
        Ok(SettingsStore {
            path,
            current: RwLock::new(current),
        })
    }

    pub fn get(&self) -> Settings {
        self.current.read().clone()
    }

    /// Validate, normalize, and persist settings. If the cloud transcription
    /// endpoint moves to a different origin, the stored cloud API key is
    /// deleted so it is never sent to a host the user did not enter it for.
    pub fn save(&self, settings: Settings) -> Result<Settings> {
        self.save_with(settings, || set_secret(SECRET_OPENAI, ""))
    }

    /// `save` with an injectable "forget the cloud API key" action (tests).
    fn save_with<F>(&self, mut settings: Settings, forget_cloud_key: F) -> Result<Settings>
    where
        F: FnOnce() -> Result<()>,
    {
        settings.chunk_seconds = settings.chunk_seconds.clamp(3, 30);
        if settings.language.trim().is_empty() {
            settings.language = "auto".into();
        }
        if settings.cloud_base_url.trim().is_empty() {
            settings.cloud_base_url = Settings::default().cloud_base_url;
        }
        settings.cloud_base_url = normalize_cloud_url(&settings.cloud_base_url)?;
        if settings.claude_model.trim().is_empty() {
            settings.claude_model = Settings::default().claude_model;
        }
        if settings.system_audio_device.as_deref().is_some_and(|d| d.trim().is_empty()) {
            settings.system_audio_device = None;
        }

        let old_url = self.current.read().cloud_base_url.clone();
        let old_origin = cloud_origin(&old_url);
        let new_origin = cloud_origin(&settings.cloud_base_url);
        if old_origin.is_none() || old_origin != new_origin {
            log::info!(
                "cloud transcription endpoint origin changed ({} -> {}); clearing the stored cloud API key",
                old_origin.as_deref().unwrap_or("<invalid>"),
                new_origin.as_deref().unwrap_or("<invalid>"),
            );
            forget_cloud_key().context("clearing the cloud API key after an endpoint change")?;
        }

        let text = serde_json::to_string_pretty(&settings)?;
        fs::write(&self.path, text)
            .with_context(|| format!("writing {}", self.path.display()))?;
        crate::fs_perms::harden_file(&self.path);
        *self.current.write() = settings.clone();
        Ok(settings)
    }
}

/// Validate a cloud transcription base URL and normalize it to
/// `scheme://host[:port]/path` with no trailing slash.
///
/// Accepts `https://` anywhere, and plain `http://` only for loopback hosts
/// (`localhost`, `127.0.0.1`, `::1`) so local servers keep working. Rejects
/// embedded credentials, query strings, and fragments.
pub fn normalize_cloud_url(raw: &str) -> Result<String> {
    let raw = raw.trim();
    let url = url::Url::parse(raw).map_err(|e| anyhow!("Cloud base URL \"{raw}\" is not a valid URL ({e})."))?;
    let host = url
        .host()
        .ok_or_else(|| anyhow!("Cloud base URL must include a host name."))?;
    match url.scheme() {
        "https" => {}
        "http" => {
            let loopback = match &host {
                url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
                url::Host::Ipv4(ip) => *ip == std::net::Ipv4Addr::LOCALHOST,
                url::Host::Ipv6(ip) => *ip == std::net::Ipv6Addr::LOCALHOST,
            };
            if !loopback {
                return Err(anyhow!(
                    "Cloud base URL must use https:// (plain http:// is only allowed for localhost, 127.0.0.1, or ::1)."
                ));
            }
        }
        other => return Err(anyhow!("Cloud base URL must use https:// (got \"{other}://\").")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(anyhow!("Cloud base URL must not contain a username or password; store the API key separately."));
    }
    if url.query().is_some() {
        return Err(anyhow!("Cloud base URL must not contain a query string (\"?...\")."));
    }
    if url.fragment().is_some() {
        return Err(anyhow!("Cloud base URL must not contain a fragment (\"#...\")."));
    }
    let host_str = url.host_str().unwrap_or_default();
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let path = url.path().trim_end_matches('/');
    Ok(format!("{}://{host_str}{port}{path}", url.scheme()))
}

/// `scheme://host[:port]` of a cloud base URL, or None if it does not parse.
fn cloud_origin(url: &str) -> Option<String> {
    url::Url::parse(url).ok().map(|u| u.origin().ascii_serialization())
}

fn validate_secret_name(name: &str) -> Result<()> {
    if name == SECRET_OPENAI || name == SECRET_ANTHROPIC {
        Ok(())
    } else {
        Err(anyhow!("unknown secret name: {name}"))
    }
}

fn entry(name: &str) -> Result<keyring::Entry> {
    validate_secret_name(name)?;
    keyring::Entry::new(KEYRING_SERVICE, name).map_err(|e| anyhow!("keychain: {e}"))
}

pub fn set_secret(name: &str, value: &str) -> Result<()> {
    let e = entry(name)?;
    let value = value.trim();
    if value.is_empty() {
        match e.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(anyhow!("keychain delete: {err}")),
        }
    } else {
        e.set_password(value).map_err(|err| anyhow!("keychain store: {err}"))
    }
}

pub fn get_secret(name: &str) -> Result<Option<String>> {
    let e = entry(name)?;
    match e.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(anyhow!("keychain read: {err}")),
    }
}

pub fn has_secret(name: &str) -> bool {
    matches!(get_secret(name), Ok(Some(_)))
}

pub fn require_secret(name: &str, what: &str) -> Result<String> {
    get_secret(name)?.ok_or_else(|| anyhow!("{what} is not set. Add it in Settings."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn accepts_and_normalizes_good_urls() {
        let ok = |raw: &str, want: &str| assert_eq!(normalize_cloud_url(raw).unwrap(), want, "{raw}");
        ok("https://api.openai.com/v1", "https://api.openai.com/v1");
        ok("  https://api.openai.com/v1/ ", "https://api.openai.com/v1");
        ok("HTTPS://API.Groq.com/openai/v1//", "https://api.groq.com/openai/v1");
        ok("https://example.com", "https://example.com");
        ok("https://example.com:443/v1", "https://example.com/v1");
        ok("https://example.com:8443/v1", "https://example.com:8443/v1");
        ok("http://localhost:8000/v1", "http://localhost:8000/v1");
        ok("http://127.0.0.1:9000", "http://127.0.0.1:9000");
        ok("http://[::1]:8080/v1/", "http://[::1]:8080/v1");
    }

    #[test]
    fn rejects_bad_urls() {
        for raw in [
            "api.openai.com/v1",
            "ftp://example.com/v1",
            "file:///etc/passwd",
            "http://example.com/v1",
            "http://192.168.1.10/v1",
            "http://10.0.0.1/v1",
            "http://127.0.0.2/v1",
            "http://localhost.evil.com/v1",
            "https://user:pass@example.com/v1",
            "https://user@example.com/v1",
            "https://example.com/v1?x=1",
            "https://example.com/v1?",
            "https://example.com/v1#frag",
            "not a url",
        ] {
            assert!(normalize_cloud_url(raw).is_err(), "should reject {raw}");
        }
    }

    fn temp_store() -> (SettingsStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("transcriptor-settings-{}", uuid::Uuid::new_v4()));
        (SettingsStore::load(&dir).unwrap(), dir)
    }

    #[test]
    fn origin_change_clears_cloud_key() {
        let (store, dir) = temp_store();
        let cleared = Cell::new(0);
        let forget = || {
            cleared.set(cleared.get() + 1);
            Ok(())
        };

        // Same origin, different path/trailing slash: key kept.
        let mut s = store.get();
        s.cloud_base_url = "https://api.openai.com/v1/".into();
        let saved = store.save_with(s, forget).unwrap();
        assert_eq!(saved.cloud_base_url, "https://api.openai.com/v1");
        let mut s = store.get();
        s.cloud_base_url = "https://api.openai.com/other".into();
        store.save_with(s, || { cleared.set(cleared.get() + 1); Ok(()) }).unwrap();
        assert_eq!(cleared.get(), 0);

        // New host: key cleared.
        let mut s = store.get();
        s.cloud_base_url = "https://api.groq.com/openai/v1".into();
        store.save_with(s, || { cleared.set(cleared.get() + 1); Ok(()) }).unwrap();
        assert_eq!(cleared.get(), 1);

        // New port on the same host: key cleared.
        let mut s = store.get();
        s.cloud_base_url = "https://api.groq.com:8443/openai/v1".into();
        store.save_with(s, || { cleared.set(cleared.get() + 1); Ok(()) }).unwrap();
        assert_eq!(cleared.get(), 2);

        // Unrelated save: key kept.
        let mut s = store.get();
        s.language = "en".into();
        store.save_with(s, || { cleared.set(cleared.get() + 1); Ok(()) }).unwrap();
        assert_eq!(cleared.get(), 2);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn system_audio_device_defaults_and_normalizes() {
        // Older settings files without the field load as automatic.
        let old: Settings = serde_json::from_str(r#"{"capture_system_audio":true}"#).unwrap();
        assert_eq!(old.system_audio_device, None);

        let (store, dir) = temp_store();
        let mut s = store.get();
        s.system_audio_device = Some("  ".into());
        assert_eq!(store.save_with(s, || Ok(())).unwrap().system_audio_device, None);
        let mut s = store.get();
        s.system_audio_device = Some("Monitor of Built-in Audio".into());
        let saved = store.save_with(s, || Ok(())).unwrap();
        assert_eq!(saved.system_audio_device.as_deref(), Some("Monitor of Built-in Audio"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejected_url_is_not_saved_and_key_untouched() {
        let (store, dir) = temp_store();
        let mut s = store.get();
        s.cloud_base_url = "http://evil.example.com/v1".into();
        let err = store
            .save_with(s, || panic!("must not clear the key for a rejected URL"))
            .unwrap_err();
        assert!(format!("{err:#}").contains("https://"));
        assert_eq!(store.get().cloud_base_url, Settings::default().cloud_base_url);
        assert!(!dir.join("settings.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_key_deletion_blocks_save() {
        let (store, dir) = temp_store();
        let mut s = store.get();
        s.cloud_base_url = "https://api.groq.com/openai/v1".into();
        assert!(store.save_with(s, || Err(anyhow!("keychain locked"))).is_err());
        assert_eq!(store.get().cloud_base_url, Settings::default().cloud_base_url);
        let _ = std::fs::remove_dir_all(dir);
    }
}
