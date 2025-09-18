use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use dirs::home_dir;
use serde::Deserialize;

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
        let file_config = load_file_config();

        let cooldown_minutes = env::var("COOLDOWN_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.data.cooldown_minutes)
            })
            .unwrap_or(0); // Default to 0 (no cooldown)

        let mode = Mode::from_env(
            env::var("COOLDOWN_MODE")
                .ok()
                .or_else(|| file_config.as_ref().and_then(|cfg| cfg.data.mode.clone())),
        );

        let ttl_seconds = env::var("COOLDOWN_TTL_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| file_config.as_ref().and_then(|cfg| cfg.data.ttl_seconds))
            .unwrap_or(86_400);

        let allowlist_path = env::var_os("COOLDOWN_ALLOWLIST_PATH")
            .map(PathBuf::from)
            .or_else(|| file_config.as_ref().and_then(|cfg| cfg.allowlist_path()))
            .filter(|path| !path.as_os_str().is_empty());

        let cache_dir = env::var_os("COOLDOWN_CACHE_DIR")
            .map(PathBuf::from)
            .or_else(|| file_config.as_ref().and_then(|cfg| cfg.cache_dir()))
            .filter(|path| !path.as_os_str().is_empty());

        let offline_ok = match env::var("COOLDOWN_OFFLINE_OK") {
            Ok(value) => parse_bool(&value),
            Err(_) => file_config
                .as_ref()
                .and_then(|cfg| cfg.data.offline_ok)
                .unwrap_or(false),
        };

        let http_retries = env::var("COOLDOWN_HTTP_RETRIES")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&v| v <= 8)
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.data.http_retries)
                    .filter(|&v| v <= 8)
            })
            .unwrap_or(2);

        let verbose = match env::var("COOLDOWN_VERBOSE") {
            Ok(value) => parse_bool(&value),
            Err(_) => file_config
                .as_ref()
                .and_then(|cfg| cfg.data.verbose)
                .unwrap_or(false),
        };

        let registry_api = env::var("COOLDOWN_REGISTRY_API")
            .ok()
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.data.registry_api.clone())
            })
            .unwrap_or_else(|| "https://crates.io/api/v1/".to_string());

        let allowed_registries = env::var("COOLDOWN_REGISTRY_INDEX")
            .ok()
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.data.registry_index.clone())
            })
            .map(|value| parse_registry_list(&value))
            .unwrap_or_else(default_allowed_registries);

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

fn parse_bool(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true")
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
struct RawFileConfig {
    #[serde(alias = "COOLDOWN_MINUTES")]
    cooldown_minutes: Option<u64>,
    #[serde(alias = "COOLDOWN_MODE")]
    mode: Option<String>,
    #[serde(alias = "COOLDOWN_ALLOWLIST_PATH")]
    allowlist_path: Option<PathBuf>,
    #[serde(alias = "COOLDOWN_TTL_SECONDS")]
    ttl_seconds: Option<u64>,
    #[serde(alias = "COOLDOWN_CACHE_DIR")]
    cache_dir: Option<PathBuf>,
    #[serde(alias = "COOLDOWN_OFFLINE_OK")]
    offline_ok: Option<bool>,
    #[serde(alias = "COOLDOWN_HTTP_RETRIES")]
    http_retries: Option<u32>,
    #[serde(alias = "COOLDOWN_VERBOSE")]
    verbose: Option<bool>,
    #[serde(alias = "COOLDOWN_REGISTRY_API")]
    registry_api: Option<String>,
    #[serde(alias = "COOLDOWN_REGISTRY_INDEX")]
    registry_index: Option<String>,
}

#[derive(Debug, Clone)]
struct FileConfig {
    path: PathBuf,
    data: RawFileConfig,
}

impl FileConfig {
    fn base_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn resolve_path(&self, candidate: &PathBuf) -> PathBuf {
        if candidate.is_absolute() {
            candidate.clone()
        } else {
            self.base_dir().join(candidate)
        }
    }

    fn allowlist_path(&self) -> Option<PathBuf> {
        self.data
            .allowlist_path
            .as_ref()
            .map(|path| self.resolve_path(path))
    }

    fn cache_dir(&self) -> Option<PathBuf> {
        self.data
            .cache_dir
            .as_ref()
            .map(|path| self.resolve_path(path))
    }
}

fn load_file_config() -> Option<FileConfig> {
    if let Some(path) = workspace_config_path() {
        return read_file_config(&path);
    }

    if let Some(path) = user_config_path() {
        return read_file_config(&path);
    }

    None
}

fn workspace_config_path() -> Option<PathBuf> {
    let Ok(current_dir) = env::current_dir() else {
        return None;
    };
    let path = current_dir.join("cooldown.toml");
    if path.exists() { Some(path) } else { None }
}

fn user_config_path() -> Option<PathBuf> {
    let home = home_dir()?;
    let path = home.join(".cargo").join("cooldown.toml");
    if path.exists() { Some(path) } else { None }
}

fn read_file_config(path: &Path) -> Option<FileConfig> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) => {
            eprintln!("Failed to read {}: {err}", path.display());
            return None;
        }
    };

    match toml::from_str::<RawFileConfig>(&contents) {
        Ok(data) => Some(FileConfig {
            path: path.to_path_buf(),
            data,
        }),
        Err(err) => {
            eprintln!("Failed to parse {}: {err}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_fs::TempDir;
    use assert_fs::prelude::*;
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

    #[test]
    fn loads_workspace_cooldown_file() {
        let _guard = env_lock().lock().unwrap();

        let workspace = TempDir::new().unwrap();
        let fake_home = TempDir::new().unwrap();
        let original_dir = env::current_dir().unwrap();
        let original_home = env::var("HOME").ok();
        let original_user = env::var("USERPROFILE").ok();

        unsafe { env::set_var("HOME", fake_home.path()) };
        unsafe { env::set_var("USERPROFILE", fake_home.path()) };
        env::set_current_dir(workspace.path()).unwrap();

        workspace
            .child("cooldown.toml")
            .write_str(
                r#"cooldown_minutes = 15
mode = "warn"
allowlist_path = "allow.toml"
offline_ok = true
verbose = true
registry_index = "https://mirror.example/index"
"#,
            )
            .unwrap();

        let config = Config::from_env();

        assert_eq!(config.cooldown_minutes, 15);
        assert_eq!(config.mode, Mode::Warn);
        assert!(config.allowlist_path.is_some());
        assert!(config.allowlist_path.unwrap().ends_with("allow.toml"));
        assert!(config.offline_ok);
        assert!(config.verbose);
        assert_eq!(
            config.allowed_registries,
            vec!["registry+https://mirror.example/index".to_string()]
        );

        env::set_current_dir(original_dir).unwrap();
        match original_home {
            Some(val) => unsafe { env::set_var("HOME", val) },
            None => unsafe { env::remove_var("HOME") },
        }
        match original_user {
            Some(val) => unsafe { env::set_var("USERPROFILE", val) },
            None => unsafe { env::remove_var("USERPROFILE") },
        }

        workspace.close().unwrap();
        fake_home.close().unwrap();
    }

    #[test]
    fn loads_user_cargo_cooldown_file_when_workspace_missing() {
        let _guard = env_lock().lock().unwrap();

        let workspace = TempDir::new().unwrap();
        let fake_home = TempDir::new().unwrap();
        let cargo_dir = fake_home.child(".cargo");
        cargo_dir.create_dir_all().unwrap();

        let original_dir = env::current_dir().unwrap();
        let original_home = env::var("HOME").ok();
        let original_user = env::var("USERPROFILE").ok();

        unsafe { env::set_var("HOME", fake_home.path()) };
        unsafe { env::set_var("USERPROFILE", fake_home.path()) };
        env::set_current_dir(workspace.path()).unwrap();

        cargo_dir
            .child("cooldown.toml")
            .write_str(
                r#"cooldown_minutes = 5
mode = "off"
http_retries = 3
"#,
            )
            .unwrap();

        let config = Config::from_env();

        assert_eq!(config.cooldown_minutes, 5);
        assert_eq!(config.mode, Mode::Off);
        assert_eq!(config.http_retries, 3);

        env::set_current_dir(original_dir).unwrap();
        match original_home {
            Some(val) => unsafe { env::set_var("HOME", val) },
            None => unsafe { env::remove_var("HOME") },
        }
        match original_user {
            Some(val) => unsafe { env::set_var("USERPROFILE", val) },
            None => unsafe { env::remove_var("USERPROFILE") },
        }

        workspace.close().unwrap();
        fake_home.close().unwrap();
    }

    #[test]
    fn uppercase_keys_are_supported_for_backwards_compat() {
        let _guard = env_lock().lock().unwrap();

        let workspace = TempDir::new().unwrap();
        let fake_home = TempDir::new().unwrap();
        let original_dir = env::current_dir().unwrap();
        let original_home = env::var("HOME").ok();
        let original_user = env::var("USERPROFILE").ok();

        unsafe { env::set_var("HOME", fake_home.path()) };
        unsafe { env::set_var("USERPROFILE", fake_home.path()) };
        env::set_current_dir(workspace.path()).unwrap();

        workspace
            .child("cooldown.toml")
            .write_str(
                r#"COOLDOWN_MINUTES = 60
COOLDOWN_VERBOSE = true
"#,
            )
            .unwrap();

        let config = Config::from_env();
        assert_eq!(config.cooldown_minutes, 60);
        assert!(config.verbose);

        env::set_current_dir(original_dir).unwrap();
        match original_home {
            Some(val) => unsafe { env::set_var("HOME", val) },
            None => unsafe { env::remove_var("HOME") },
        }
        match original_user {
            Some(val) => unsafe { env::set_var("USERPROFILE", val) },
            None => unsafe { env::remove_var("USERPROFILE") },
        }

        workspace.close().unwrap();
        fake_home.close().unwrap();
    }

    #[test]
    fn environment_overrides_file_configuration() {
        let _guard = env_lock().lock().unwrap();

        let workspace = TempDir::new().unwrap();
        let fake_home = TempDir::new().unwrap();
        let original_dir = env::current_dir().unwrap();
        let original_home = env::var("HOME").ok();
        let original_user = env::var("USERPROFILE").ok();
        let original_minutes = env::var("COOLDOWN_MINUTES").ok();

        unsafe { env::set_var("HOME", fake_home.path()) };
        unsafe { env::set_var("USERPROFILE", fake_home.path()) };
        env::set_current_dir(workspace.path()).unwrap();

        workspace
            .child("cooldown.toml")
            .write_str("cooldown_minutes = 30\n")
            .unwrap();

        unsafe { env::set_var("COOLDOWN_MINUTES", "10") };
        let config = Config::from_env();
        assert_eq!(config.cooldown_minutes, 10);

        env::set_current_dir(original_dir).unwrap();
        match original_home {
            Some(val) => unsafe { env::set_var("HOME", val) },
            None => unsafe { env::remove_var("HOME") },
        }
        match original_user {
            Some(val) => unsafe { env::set_var("USERPROFILE", val) },
            None => unsafe { env::remove_var("USERPROFILE") },
        }
        match original_minutes {
            Some(val) => unsafe { env::set_var("COOLDOWN_MINUTES", val) },
            None => unsafe { env::remove_var("COOLDOWN_MINUTES") },
        }

        workspace.close().unwrap();
        fake_home.close().unwrap();
    }
}
