//! Settings persistence (JSON file) and API-key storage (OS keychain).

use crate::models::Settings;
use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use std::fs;
use std::path::{Path, PathBuf};

const KEYRING_SERVICE: &str = "com.justinleahy.transcriptor";
pub const SECRET_OPENAI: &str = "openai_api_key";
pub const SECRET_ANTHROPIC: &str = "anthropic_api_key";
pub const SECRET_SECONDBRAIN: &str = "secondbrain_api_key";

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
    /// endpoint (or the SecondBrain endpoint) moves to a different origin, the
    /// matching stored API key is deleted so it is never sent to a host the
    /// user did not enter it for.
    pub fn save(&self, settings: Settings) -> Result<Settings> {
        self.save_with(settings, |name| set_secret(name, ""))
    }

    /// `save` with an injectable "forget this API key" action (tests). The
    /// closure receives the secret name to delete.
    fn save_with<F>(&self, mut settings: Settings, mut forget_key: F) -> Result<Settings>
    where
        F: FnMut(&str) -> Result<()>,
    {
        settings.chunk_seconds = settings.chunk_seconds.clamp(3, 30);
        if settings.language.trim().is_empty() {
            settings.language = "auto".into();
        }
        if settings.cloud_base_url.trim().is_empty() {
            settings.cloud_base_url = Settings::default().cloud_base_url;
        }
        settings.cloud_base_url = normalize_cloud_url(&settings.cloud_base_url)?;
        if settings.secondbrain_url.trim().is_empty() {
            settings.secondbrain_url = Settings::default().secondbrain_url;
        }
        // Validated here, with the whole new settings in hand, because what it
        // accepts depends on `secondbrain_allow_private_http`.
        settings.secondbrain_url =
            normalize_secondbrain_url(&settings.secondbrain_url, settings.secondbrain_allow_private_http)?;
        settings.secondbrain_tags = normalize_tags(&settings.secondbrain_tags);
        settings.secondbrain_identity = normalize_identity(&settings.secondbrain_identity);
        if settings.claude_model.trim().is_empty() {
            settings.claude_model = Settings::default().claude_model;
        }
        if settings.system_audio_device.as_deref().is_some_and(|d| d.trim().is_empty()) {
            settings.system_audio_device = None;
        }

        let (old_cloud, old_brain) = {
            let cur = self.current.read();
            (cur.cloud_base_url.clone(), cur.secondbrain_url.clone())
        };
        for (what, secret, old_url, new_url) in [
            ("cloud transcription", SECRET_OPENAI, &old_cloud, &settings.cloud_base_url),
            ("SecondBrain", SECRET_SECONDBRAIN, &old_brain, &settings.secondbrain_url),
        ] {
            let old_origin = url_origin(old_url);
            let new_origin = url_origin(new_url);
            if old_origin.is_none() || old_origin != new_origin {
                log::info!(
                    "{what} endpoint origin changed ({} -> {}); clearing the stored {what} API key",
                    old_origin.as_deref().unwrap_or("<invalid>"),
                    new_origin.as_deref().unwrap_or("<invalid>"),
                );
                forget_key(secret)
                    .with_context(|| format!("clearing the {what} API key after an endpoint change"))?;
            }
        }

        let text = serde_json::to_string_pretty(&settings)?;
        fs::write(&self.path, text)
            .with_context(|| format!("writing {}", self.path.display()))?;
        crate::fs_perms::harden_file(&self.path);
        *self.current.write() = settings.clone();
        Ok(settings)
    }
}

/// Validate a cloud transcription base URL (see `normalize_service_url`).
pub fn normalize_cloud_url(raw: &str) -> Result<String> {
    normalize_service_url(raw, &UrlRules { label: "Cloud base URL", private_http: PrivateHttp::NotOffered })
}

/// Validate the SecondBrain base URL. Plain `http://` to private-network hosts
/// is accepted when `allow_private_http` is set (spec v0.4 §7.3: plain HTTP
/// inside the VPN).
pub fn normalize_secondbrain_url(raw: &str, allow_private_http: bool) -> Result<String> {
    let private_http = if allow_private_http { PrivateHttp::Allowed } else { PrivateHttp::Disallowed };
    normalize_service_url(raw, &UrlRules { label: "SecondBrain URL", private_http })
}

/// Whether plain `http://` to private-network hosts is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateHttp {
    /// Never, and the setting does not exist for this URL.
    NotOffered,
    /// Not now; the user can turn it on.
    Disallowed,
    Allowed,
}

/// How `normalize_service_url` validates one setting.
#[derive(Debug, Clone, Copy)]
pub struct UrlRules<'a> {
    /// Names the setting in error messages.
    pub label: &'a str,
    pub private_http: PrivateHttp,
}

/// Name of the opt-in setting, as shown in the UI.
const PRIVATE_HTTP_SETTING: &str = "Allow plain HTTP on my private network";

/// Domain suffixes that only resolve inside a private network.
const PRIVATE_DOMAIN_SUFFIXES: &[&str] = &[".lan", ".local", ".internal", ".home", ".home.arpa", ".localdomain"];

/// `localhost`, `127.0.0.1`, or `::1`.
fn is_loopback_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Domain(d) => d.trim_end_matches('.').eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(ip) => *ip == std::net::Ipv4Addr::LOCALHOST,
        url::Host::Ipv6(ip) => *ip == std::net::Ipv6Addr::LOCALHOST,
    }
}

/// RFC1918 or link-local IPv4, IPv6 ULA (fc00::/7) or link-local (fe80::/10),
/// a single-label host name, or a name under a private-only suffix.
pub fn is_private_network_host(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Ipv4(ip) => ip.is_private() || ip.is_link_local(),
        url::Host::Ipv6(ip) => {
            let first = ip.segments()[0];
            (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
        url::Host::Domain(d) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            if d.is_empty() {
                return false;
            }
            !d.contains('.')
                || PRIVATE_DOMAIN_SUFFIXES
                    .iter()
                    .any(|suffix| d.ends_with(suffix) && d.len() > suffix.len())
        }
    }
}

/// Validate an HTTP service base URL and normalize it to
/// `scheme://host[:port]/path` with no trailing slash.
///
/// Accepts `https://` anywhere and plain `http://` for loopback hosts
/// (`localhost`, `127.0.0.1`, `::1`) so local servers keep working. With
/// `PrivateHttp::Allowed`, plain `http://` is also accepted for private-network
/// hosts (see `is_private_network_host`). Rejects embedded credentials, query
/// strings, and fragments.
pub fn normalize_service_url(raw: &str, rules: &UrlRules<'_>) -> Result<String> {
    let label = rules.label;
    let raw = raw.trim();
    let url = url::Url::parse(raw).map_err(|e| anyhow!("{label} \"{raw}\" is not a valid URL ({e})."))?;
    let host = url
        .host()
        .ok_or_else(|| anyhow!("{label} must include a host name."))?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback_host(&host) => {}
        "http" => {
            let private = is_private_network_host(&host);
            match (rules.private_http, private) {
                (PrivateHttp::Allowed, true) => {}
                (PrivateHttp::NotOffered, _) => {
                    return Err(anyhow!(
                        "{label} must use https:// (plain http:// is only allowed for localhost, 127.0.0.1, or ::1)."
                    ))
                }
                (PrivateHttp::Disallowed, true) => {
                    return Err(anyhow!(
                        "{label} must use https://. Plain http:// to a private-network host like {host} is allowed \
                         only if you turn on \"{PRIVATE_HTTP_SETTING}\"."
                    ))
                }
                (_, false) => {
                    return Err(anyhow!(
                        "{label} must use https://. Plain http:// is only allowed for localhost, 127.0.0.1, ::1, \
                         or (with \"{PRIVATE_HTTP_SETTING}\") private addresses such as 192.168.x.x, 10.x.x.x, \
                         or *.lan / *.local names; {host} is not one."
                    ))
                }
            }
        }
        other => return Err(anyhow!("{label} must use https:// (got \"{other}://\").")),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(anyhow!("{label} must not contain a username or password; store the API key separately."));
    }
    if url.query().is_some() {
        return Err(anyhow!("{label} must not contain a query string (\"?...\")."));
    }
    if url.fragment().is_some() {
        return Err(anyhow!("{label} must not contain a fragment (\"#...\")."));
    }
    let host_str = url.host_str().unwrap_or_default();
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let path = url.path().trim_end_matches('/');
    Ok(format!("{}://{host_str}{port}{path}", url.scheme()))
}

/// `scheme://host[:port]` of a base URL, or None if it does not parse.
fn url_origin(url: &str) -> Option<String> {
    url::Url::parse(url).ok().map(|u| u.origin().ascii_serialization())
}

/// The identity on one line, at most 200 characters.
fn normalize_identity(raw: &str) -> String {
    let one = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(200).collect()
}

/// Trim tags, drop empty ones and case-insensitive duplicates, keep order.
fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        let t = t.trim();
        if !t.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(t)) {
            out.push(t.to_string());
        }
    }
    out
}

fn validate_secret_name(name: &str) -> Result<()> {
    if name == SECRET_OPENAI || name == SECRET_ANTHROPIC || name == SECRET_SECONDBRAIN {
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
        let forget = |_: &str| {
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
        store.save_with(s, |_| { cleared.set(cleared.get() + 1); Ok(()) }).unwrap();
        assert_eq!(cleared.get(), 0);

        // New host: key cleared.
        let mut s = store.get();
        s.cloud_base_url = "https://api.groq.com/openai/v1".into();
        store.save_with(s, |_| { cleared.set(cleared.get() + 1); Ok(()) }).unwrap();
        assert_eq!(cleared.get(), 1);

        // New port on the same host: key cleared.
        let mut s = store.get();
        s.cloud_base_url = "https://api.groq.com:8443/openai/v1".into();
        store.save_with(s, |_| { cleared.set(cleared.get() + 1); Ok(()) }).unwrap();
        assert_eq!(cleared.get(), 2);

        // Unrelated save: key kept.
        let mut s = store.get();
        s.language = "en".into();
        store.save_with(s, |_| { cleared.set(cleared.get() + 1); Ok(()) }).unwrap();
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
        assert_eq!(store.save_with(s, |_| Ok(())).unwrap().system_audio_device, None);
        let mut s = store.get();
        s.system_audio_device = Some("Monitor of Built-in Audio".into());
        let saved = store.save_with(s, |_| Ok(())).unwrap();
        assert_eq!(saved.system_audio_device.as_deref(), Some("Monitor of Built-in Audio"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejected_url_is_not_saved_and_key_untouched() {
        let (store, dir) = temp_store();
        let mut s = store.get();
        s.cloud_base_url = "http://evil.example.com/v1".into();
        let err = store
            .save_with(s, |_| panic!("must not clear the key for a rejected URL"))
            .unwrap_err();
        assert!(format!("{err:#}").contains("https://"));
        assert_eq!(store.get().cloud_base_url, Settings::default().cloud_base_url);
        assert!(!dir.join("settings.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn secondbrain_settings_default_validate_and_clear_key() {
        // Older settings files load with SecondBrain defaults.
        let old: Settings = serde_json::from_str(r#"{"language":"en"}"#).unwrap();
        assert!(!old.secondbrain_enabled);
        assert_eq!(old.secondbrain_url, "http://127.0.0.1:7171/v1");
        assert!(old.secondbrain_auto_export);
        assert_eq!(old.secondbrain_tags, vec!["meeting", "transcriptor"]);
        assert!(!old.secondbrain_wait_for_index);
        assert_eq!(old.secondbrain_identity, "");
        assert!(!old.secondbrain_allow_private_http);

        let (store, dir) = temp_store();
        let forgotten = std::cell::RefCell::new(Vec::<String>::new());

        // Normalized; same origin keeps the key; tags cleaned up.
        let mut s = store.get();
        s.secondbrain_url = " http://localhost:7171/v1/ ".into();
        s.secondbrain_tags = vec![" meeting ".into(), "".into(), "Meeting".into(), "work".into()];
        let saved = store
            .save_with(s, |n| { forgotten.borrow_mut().push(n.to_string()); Ok(()) })
            .unwrap();
        assert_eq!(saved.secondbrain_url, "http://localhost:7171/v1");
        assert_eq!(saved.secondbrain_tags, vec!["meeting", "work"]);
        // 127.0.0.1 -> localhost is a different origin.
        assert_eq!(*forgotten.borrow(), vec![SECRET_SECONDBRAIN.to_string()]);

        // Empty falls back to the default.
        let mut s = store.get();
        s.secondbrain_url = "  ".into();
        assert_eq!(store.save_with(s, |_| Ok(())).unwrap().secondbrain_url, "http://127.0.0.1:7171/v1");

        // Plain http to a non-loopback host is rejected, with the right label.
        let mut s = store.get();
        s.secondbrain_url = "http://brain.example.com/v1".into();
        let err = store.save_with(s, |_| panic!("rejected URL must not clear keys")).unwrap_err();
        assert!(format!("{err:#}").contains("SecondBrain URL"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn secondbrain_url_matrix() {
        // Accepted whatever the private-HTTP setting is.
        for allow in [false, true] {
            for (raw, want) in [
                ("https://brain.example.com/v1/", "https://brain.example.com/v1"),
                ("https://192.168.1.10:7171/v1", "https://192.168.1.10:7171/v1"),
                ("http://localhost:7171/v1", "http://localhost:7171/v1"),
                ("http://127.0.0.1:7171/v1", "http://127.0.0.1:7171/v1"),
                ("http://[::1]:7171/v1", "http://[::1]:7171/v1"),
            ] {
                assert_eq!(normalize_secondbrain_url(raw, allow).unwrap(), want, "{raw} allow={allow}");
            }
        }
        // Private-network http: only with the setting on.
        for (raw, want) in [
            ("http://brain.lan:7171/v1", "http://brain.lan:7171/v1"),
            ("http://BRAIN.LAN:7171/v1/", "http://brain.lan:7171/v1"),
            ("http://brain.local/v1", "http://brain.local/v1"),
            ("http://nas.internal/v1", "http://nas.internal/v1"),
            ("http://brain.home/v1", "http://brain.home/v1"),
            ("http://brain.home.arpa/v1", "http://brain.home.arpa/v1"),
            ("http://brain.localdomain/v1", "http://brain.localdomain/v1"),
            ("http://brain:7171/v1", "http://brain:7171/v1"),
            ("http://10.0.0.5:7171/v1", "http://10.0.0.5:7171/v1"),
            ("http://172.16.0.1/v1", "http://172.16.0.1/v1"),
            ("http://172.31.255.254/v1", "http://172.31.255.254/v1"),
            ("http://192.168.1.10:7171/v1", "http://192.168.1.10:7171/v1"),
            ("http://169.254.10.20/v1", "http://169.254.10.20/v1"),
            ("http://[fd12:3456::1]:7171/v1", "http://[fd12:3456::1]:7171/v1"),
            ("http://[fc00::1]/v1", "http://[fc00::1]/v1"),
            ("http://[fe80::1]/v1", "http://[fe80::1]/v1"),
        ] {
            assert_eq!(normalize_secondbrain_url(raw, true).unwrap(), want, "{raw}");
            let err = normalize_secondbrain_url(raw, false).unwrap_err().to_string();
            assert!(err.contains("Allow plain HTTP on my private network"), "{raw}: {err}");
        }
        // Never accepted.
        for raw in [
            "http://brain.example.com/v1",
            "http://8.8.8.8/v1",
            "http://172.32.0.1/v1",
            "http://172.15.0.1/v1",
            "http://11.0.0.1/v1",
            "http://127.0.0.2/v1",
            "http://[2001:db8::1]/v1",
            "http://brain.lan.example.com/v1",
            "http://lan.com/v1",
            "http://localhost.evil.com/v1",
            "http://evil.com/brain.lan",
            "ftp://brain.lan/v1",
            "http://user:pw@brain.lan/v1",
            "http://brain.lan/v1#x",
        ] {
            assert!(normalize_secondbrain_url(raw, true).is_err(), "should reject {raw}");
            assert!(normalize_secondbrain_url(raw, false).is_err(), "should reject {raw}");
        }
        // The cloud URL never allows private http, and doesn't mention the setting.
        let err = normalize_cloud_url("http://192.168.1.10/v1").unwrap_err().to_string();
        assert!(err.contains("only allowed for localhost") && !err.contains("private network"), "{err}");
        let err = normalize_secondbrain_url("http://brain.example.com/v1", true).unwrap_err().to_string();
        assert!(err.contains("brain.example.com is not one"), "{err}");
    }

    #[test]
    fn private_http_setting_controls_save() {
        let (store, dir) = temp_store();
        let mut s = store.get();
        s.secondbrain_url = "http://brain.lan:7171/v1".into();
        s.secondbrain_allow_private_http = false;
        assert!(store.save_with(s.clone(), |_| panic!("rejected URL must not clear keys")).is_err());
        s.secondbrain_allow_private_http = true;
        s.secondbrain_identity = "  Justin \n Leahy ".into();
        let saved = store.save_with(s, |_| Ok(())).unwrap();
        assert_eq!(saved.secondbrain_url, "http://brain.lan:7171/v1");
        assert_eq!(saved.secondbrain_identity, "Justin Leahy");
        // Turning the option off again while keeping the URL is rejected.
        let mut s = store.get();
        s.secondbrain_allow_private_http = false;
        assert!(store.save_with(s, |_| Ok(())).is_err());
        assert!(store.get().secondbrain_allow_private_http);
        let mut s = store.get();
        s.secondbrain_identity = "x".repeat(500);
        assert_eq!(store.save_with(s, |_| Ok(())).unwrap().secondbrain_identity.len(), 200);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn secondbrain_secret_is_allowed() {
        assert!(validate_secret_name(SECRET_SECONDBRAIN).is_ok());
        assert!(validate_secret_name("nope").is_err());
    }

    #[test]
    fn failed_key_deletion_blocks_save() {
        let (store, dir) = temp_store();
        let mut s = store.get();
        s.cloud_base_url = "https://api.groq.com/openai/v1".into();
        assert!(store.save_with(s, |_| Err(anyhow!("keychain locked"))).is_err());
        assert_eq!(store.get().cloud_base_url, Settings::default().cloud_base_url);
        let _ = std::fs::remove_dir_all(dir);
    }
}
