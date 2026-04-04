use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use dirs::home_dir;
use serde::Deserialize;

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
    pub now_override: Option<DateTime<Utc>>,
    pub ttl_seconds: u64,
    pub allowlist_path: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub http_retries: u32,
    pub verbose: bool,
    pub skip_registries: Vec<String>,
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
            .unwrap_or(0);

        let mode = Mode::from_env(
            env::var("COOLDOWN_MODE")
                .ok()
                .or_else(|| file_config.as_ref().and_then(|cfg| cfg.data.mode.clone())),
        );

        let now_override = env::var("COOLDOWN_NOW")
            .ok()
            .and_then(|value| parse_datetime(&value))
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.data.now.clone())
                    .and_then(|value| parse_datetime(&value))
            });

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

        let skip_registries = env::var("COOLDOWN_SKIP_REGISTRIES")
            .ok()
            .map(|value| parse_registry_skip_list(&value))
            .or_else(|| {
                file_config
                    .as_ref()
                    .and_then(|cfg| cfg.data.skip_registries.clone())
                    .map(StringList::into_vec)
            })
            .unwrap_or_default();

        Self {
            cooldown_minutes,
            mode,
            now_override,
            ttl_seconds,
            allowlist_path,
            cache_dir,
            http_retries,
            verbose,
            skip_registries,
        }
    }
}

fn parse_registry_skip_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_bool(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true")
}

fn parse_datetime(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum StringList {
    String(String),
    List(Vec<String>),
}

impl StringList {
    fn into_vec(self) -> Vec<String> {
        match self {
            StringList::String(value) => parse_registry_skip_list(&value),
            StringList::List(values) => values
                .into_iter()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
struct RawFileConfig {
    #[serde(alias = "COOLDOWN_MINUTES")]
    cooldown_minutes: Option<u64>,
    #[serde(alias = "COOLDOWN_MODE")]
    mode: Option<String>,
    #[serde(alias = "COOLDOWN_NOW")]
    now: Option<String>,
    #[serde(alias = "COOLDOWN_ALLOWLIST_PATH")]
    allowlist_path: Option<PathBuf>,
    #[serde(alias = "COOLDOWN_TTL_SECONDS")]
    ttl_seconds: Option<u64>,
    #[serde(alias = "COOLDOWN_CACHE_DIR")]
    cache_dir: Option<PathBuf>,
    #[serde(alias = "COOLDOWN_HTTP_RETRIES")]
    http_retries: Option<u32>,
    #[serde(alias = "COOLDOWN_VERBOSE")]
    verbose: Option<bool>,
    #[serde(alias = "COOLDOWN_SKIP_REGISTRIES")]
    skip_registries: Option<StringList>,
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
    let cargo_home = cargo_home_dir()?;
    let path = cargo_home.join("cooldown.toml");
    if path.exists() { Some(path) } else { None }
}

fn cargo_home_dir() -> Option<PathBuf> {
    env::var_os("CARGO_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".cargo")))
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
    fn skip_registries_support_comma_separated_env() {
        with_env_var(
            "COOLDOWN_SKIP_REGISTRIES",
            Some("crates-io, sparse+https://codeartifact.example/index , mirror"),
            || {
                let config = Config::from_env();
                assert_eq!(
                    config.skip_registries,
                    vec![
                        "crates-io".to_string(),
                        "sparse+https://codeartifact.example/index".to_string(),
                        "mirror".to_string(),
                    ]
                );
            },
        );
    }

    #[test]
    fn cooldown_now_parses_rfc3339_override() {
        with_env_var("COOLDOWN_NOW", Some("2026-04-03T00:00:00Z"), || {
            let config = Config::from_env();
            assert_eq!(
                config.now_override,
                Some(
                    DateTime::parse_from_rfc3339("2026-04-03T00:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc)
                )
            );
        });
    }

    #[test]
    fn loads_workspace_cooldown_file() {
        let _guard = env_lock().lock().unwrap();

        let workspace = TempDir::new().unwrap();
        let fake_home = TempDir::new().unwrap();
        let original_dir = env::current_dir().unwrap();
        let original_cargo_home = env::var_os("CARGO_HOME");
        let original_home = env::var("HOME").ok();
        let original_user = env::var("USERPROFILE").ok();

        unsafe { env::remove_var("CARGO_HOME") };
        unsafe { env::set_var("HOME", fake_home.path()) };
        unsafe { env::set_var("USERPROFILE", fake_home.path()) };
        env::set_current_dir(workspace.path()).unwrap();

        workspace
            .child("cooldown.toml")
            .write_str(
                r#"cooldown_minutes = 15
mode = "warn"
allowlist_path = "allow.toml"
skip_registries = ["crates-io", "mirror"]
verbose = true
"#,
            )
            .unwrap();

        let config = Config::from_env();

        assert_eq!(config.cooldown_minutes, 15);
        assert_eq!(config.mode, Mode::Warn);
        assert!(config.allowlist_path.is_some());
        assert!(config.allowlist_path.unwrap().ends_with("allow.toml"));
        assert_eq!(
            config.skip_registries,
            vec!["crates-io".to_string(), "mirror".to_string()]
        );
        assert!(config.verbose);

        env::set_current_dir(original_dir).unwrap();
        match original_cargo_home {
            Some(val) => unsafe { env::set_var("CARGO_HOME", val) },
            None => unsafe { env::remove_var("CARGO_HOME") },
        }
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
        let original_cargo_home = env::var_os("CARGO_HOME");
        let original_home = env::var("HOME").ok();
        let original_user = env::var("USERPROFILE").ok();

        unsafe { env::remove_var("CARGO_HOME") };
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
        match original_cargo_home {
            Some(val) => unsafe { env::set_var("CARGO_HOME", val) },
            None => unsafe { env::remove_var("CARGO_HOME") },
        }
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
        let original_dir = env::current_dir().unwrap();
        let original_mode = env::var("COOLDOWN_MODE").ok();
        let original_skips = env::var("COOLDOWN_SKIP_REGISTRIES").ok();

        env::set_current_dir(workspace.path()).unwrap();
        workspace
            .child("cooldown.toml")
            .write_str(
                r#"mode = "warn"
skip_registries = ["from-file"]
"#,
            )
            .unwrap();

        unsafe { env::set_var("COOLDOWN_MODE", "off") };
        unsafe { env::set_var("COOLDOWN_SKIP_REGISTRIES", "from-env") };

        let config = Config::from_env();
        assert_eq!(config.mode, Mode::Off);
        assert_eq!(config.skip_registries, vec!["from-env".to_string()]);

        env::set_current_dir(original_dir).unwrap();
        match original_mode {
            Some(val) => unsafe { env::set_var("COOLDOWN_MODE", val) },
            None => unsafe { env::remove_var("COOLDOWN_MODE") },
        }
        match original_skips {
            Some(val) => unsafe { env::set_var("COOLDOWN_SKIP_REGISTRIES", val) },
            None => unsafe { env::remove_var("COOLDOWN_SKIP_REGISTRIES") },
        }

        workspace.close().unwrap();
    }

    #[test]
    fn uppercase_keys_are_supported_for_backwards_compat() {
        let _guard = env_lock().lock().unwrap();

        let workspace = TempDir::new().unwrap();
        let original_dir = env::current_dir().unwrap();
        env::set_current_dir(workspace.path()).unwrap();

        workspace
            .child("cooldown.toml")
            .write_str(
                r#"COOLDOWN_MINUTES = 9
COOLDOWN_MODE = "warn"
COOLDOWN_SKIP_REGISTRIES = "crates-io,mirror"
"#,
            )
            .unwrap();

        let config = Config::from_env();
        assert_eq!(config.cooldown_minutes, 9);
        assert_eq!(config.mode, Mode::Warn);
        assert_eq!(
            config.skip_registries,
            vec!["crates-io".to_string(), "mirror".to_string()]
        );

        env::set_current_dir(original_dir).unwrap();
        workspace.close().unwrap();
    }
}
