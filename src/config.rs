use std::env;
use std::path::PathBuf;

const DEFAULT_REGISTRY_INDEX: &str = "registry+https://github.com/rust-lang/crates.io-index";
const DEFAULT_SPARSE_REGISTRY_INDEX: &str = "registry+sparse+https://index.crates.io/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Enforce,
    Warn,
    Off,
}

impl Mode {
    pub fn from_env(value: Option<String>) -> Self {
        match value.as_deref() {
            Some("warn") => Mode::Warn,
            Some("off") => Mode::Off,
            _ => Mode::Enforce,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub cooldown_minutes: u64,
    pub mode: Mode,
    pub ttl_seconds: u64,
    pub allowlist_path: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub offline_ok: bool,
    pub http_retries: u32,
    pub verbose: bool,
    pub registry_api: String,
    pub allowed_registries: Vec<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let cooldown_minutes = env::var("COOLDOWN_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0); // Default to 0 (no cooldown)

        let mode = Mode::from_env(env::var("COOLDOWN_MODE").ok());

        let ttl_seconds = env::var("COOLDOWN_TTL_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(86_400);

        let allowlist_path = env::var_os("COOLDOWN_ALLOWLIST_PATH").map(PathBuf::from);

        let cache_dir = env::var_os("COOLDOWN_CACHE_DIR").map(PathBuf::from);

        let offline_ok = env::var("COOLDOWN_OFFLINE_OK")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let http_retries = env::var("COOLDOWN_HTTP_RETRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v <= 8)
            .unwrap_or(2);

        let verbose = env::var("COOLDOWN_VERBOSE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let registry_api = env::var("COOLDOWN_REGISTRY_API")
            .unwrap_or_else(|_| "https://crates.io/api/v1/".to_string());

        let allowed_registries = env::var("COOLDOWN_REGISTRY_INDEX")
            .map(|value| parse_registry_list(&value))
            .unwrap_or_else(|_| default_allowed_registries());

        Self {
            cooldown_minutes,
            mode,
            ttl_seconds,
            allowlist_path,
            cache_dir,
            offline_ok,
            http_retries,
            verbose,
            registry_api,
            allowed_registries,
        }
    }

    pub fn is_registry_allowed(&self, source: &str) -> bool {
        self.allowed_registries
            .iter()
            .any(|allowed| allowed == source)
    }
}

fn normalize_registry_index(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return DEFAULT_REGISTRY_INDEX.to_string();
    }
    if trimmed.starts_with("registry+") {
        trimmed.to_string()
    } else {
        format!("registry+{trimmed}")
    }
}

fn parse_registry_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .map(normalize_registry_index)
        .collect()
}

fn default_allowed_registries() -> Vec<String> {
    vec![
        DEFAULT_REGISTRY_INDEX.to_string(),
        DEFAULT_SPARSE_REGISTRY_INDEX.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn with_env_var<F: FnOnce()>(key: &str, value: Option<&str>, f: F) {
        let _guard = env_lock().lock().unwrap();
        let previous = env::var(key).ok();
        match value {
            Some(val) => unsafe { env::set_var(key, val) },
            None => unsafe { env::remove_var(key) },
        }
        f();
        match previous {
            Some(val) => unsafe { env::set_var(key, val) },
            None => unsafe { env::remove_var(key) },
        }
    }

    #[test]
    fn default_allowed_registries_include_sparse_and_git() {
        with_env_var("COOLDOWN_REGISTRY_INDEX", None, || {
            let config = Config::from_env();
            assert_eq!(config.allowed_registries, default_allowed_registries());
        });
    }

    #[test]
    fn registry_index_normalizes_missing_prefix() {
        with_env_var(
            "COOLDOWN_REGISTRY_INDEX",
            Some("https://example.com/custom-index"),
            || {
                let config = Config::from_env();
                assert_eq!(
                    config.allowed_registries,
                    vec!["registry+https://example.com/custom-index".to_string()]
                );
            },
        );
    }

    #[test]
    fn registry_index_respects_existing_prefix() {
        with_env_var(
            "COOLDOWN_REGISTRY_INDEX",
            Some("registry+https://alt.example.com/index"),
            || {
                let config = Config::from_env();
                assert_eq!(
                    config.allowed_registries,
                    vec!["registry+https://alt.example.com/index".to_string()]
                );
            },
        );
    }

    #[test]
    fn registry_index_supports_comma_separated_list() {
        with_env_var(
            "COOLDOWN_REGISTRY_INDEX",
            Some("registry+sparse+https://index.crates.io/, https://alt.example.com/index"),
            || {
                let config = Config::from_env();
                assert_eq!(
                    config.allowed_registries,
                    vec![
                        "registry+sparse+https://index.crates.io/".to_string(),
                        "registry+https://alt.example.com/index".to_string(),
                    ]
                );
            },
        );
    }
}
