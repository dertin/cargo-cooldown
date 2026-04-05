use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use dirs::home_dir;
use serde::Deserialize;

use crate::allowlist::{AllowSection, Allowlist};
use crate::project::ProjectContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Enforce,
    Warn,
    Off,
}

impl Mode {
    pub fn from_env(value: Option<&str>) -> Self {
        match value {
            Some("warn") => Mode::Warn,
            Some("off") => Mode::Off,
            _ => Mode::Enforce,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockfilePolicy {
    Changed,
    All,
}

impl LockfilePolicy {
    pub fn from_env(value: Option<&str>) -> Self {
        match value {
            Some("all") => LockfilePolicy::All,
            _ => LockfilePolicy::Changed,
        }
    }

    pub fn applies_to_existing_lockfile(self) -> bool {
        matches!(self, LockfilePolicy::All)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub cooldown_minutes: u64,
    pub mode: Mode,
    pub lockfile_policy: LockfilePolicy,
    pub now_override: Option<DateTime<Utc>>,
    pub ttl_seconds: u64,
    pub cache_dir: Option<PathBuf>,
    pub http_retries: u32,
    pub verbose: bool,
    pub skip_registries: Vec<String>,
    pub allowlist: Allowlist,
}

impl Config {
    pub fn load(project: &ProjectContext) -> Self {
        let mut merged = MergedConfig::default();

        if let Some(path) = user_config_path() {
            merged.apply_file(&path);
        }
        merged.apply_file(&project.workspace_config_path());
        if let Some(path) = project.member_config_path() {
            merged.apply_file(&path);
        }
        merged.apply_env();
        merged.finish()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
struct CooldownFile {
    #[serde(alias = "COOLDOWN_MINUTES")]
    cooldown_minutes: Option<u64>,
    #[serde(alias = "COOLDOWN_MODE")]
    mode: Option<String>,
    #[serde(alias = "COOLDOWN_LOCKFILE_POLICY")]
    lockfile_policy: Option<String>,
    #[serde(alias = "COOLDOWN_NOW")]
    now: Option<String>,
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
    #[serde(default)]
    allow: AllowSection,
}

#[derive(Debug, Default)]
struct MergedConfig {
    cooldown_minutes: Option<u64>,
    mode: Option<Mode>,
    lockfile_policy: Option<LockfilePolicy>,
    now_override: Option<DateTime<Utc>>,
    ttl_seconds: Option<u64>,
    cache_dir: Option<PathBuf>,
    http_retries: Option<u32>,
    verbose: Option<bool>,
    skip_registries: Vec<String>,
    allowlist: Allowlist,
}

impl MergedConfig {
    fn apply_file(&mut self, path: &Path) {
        let Some(file) = read_file_config(path) else {
            return;
        };

        if let Some(minutes) = file.data.cooldown_minutes {
            self.cooldown_minutes = Some(minutes);
        }
        if let Some(mode) = file.data.mode.as_deref() {
            self.mode = Some(Mode::from_env(Some(mode)));
        }
        if let Some(policy) = file.data.lockfile_policy.as_deref() {
            self.lockfile_policy = Some(LockfilePolicy::from_env(Some(policy)));
        }
        if let Some(now) = file.data.now.as_deref().and_then(parse_datetime) {
            self.now_override = Some(now);
        }
        if let Some(ttl_seconds) = file.data.ttl_seconds {
            self.ttl_seconds = Some(ttl_seconds);
        }
        if let Some(cache_dir) = file.data.cache_dir.as_ref() {
            self.cache_dir = Some(file.resolve_path(cache_dir));
        }
        if let Some(http_retries) = file.data.http_retries.filter(|&value| value <= 8) {
            self.http_retries = Some(http_retries);
        }
        if let Some(verbose) = file.data.verbose {
            self.verbose = Some(verbose);
        }
        if let Some(skip_registries) = file.data.skip_registries.clone() {
            self.skip_registries =
                merge_registry_skip_lists(&self.skip_registries, &skip_registries.into_vec());
        }

        self.allowlist.merge_from(&Allowlist {
            allow: file.data.allow.clone(),
        });
    }

    fn apply_env(&mut self) {
        if let Some(minutes) = env::var("COOLDOWN_MINUTES")
            .ok()
            .and_then(|value| value.parse().ok())
        {
            self.cooldown_minutes = Some(minutes);
        }
        if let Ok(value) = env::var("COOLDOWN_MODE") {
            self.mode = Some(Mode::from_env(Some(&value)));
        }
        if let Ok(value) = env::var("COOLDOWN_LOCKFILE_POLICY") {
            self.lockfile_policy = Some(LockfilePolicy::from_env(Some(&value)));
        }
        if let Some(now) = env::var("COOLDOWN_NOW")
            .ok()
            .as_deref()
            .and_then(parse_datetime)
        {
            self.now_override = Some(now);
        }
        if let Some(ttl_seconds) = env::var("COOLDOWN_TTL_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
        {
            self.ttl_seconds = Some(ttl_seconds);
        }
        if let Some(cache_dir) = env::var_os("COOLDOWN_CACHE_DIR")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
        {
            self.cache_dir = Some(cache_dir);
        }
        if let Some(http_retries) = env::var("COOLDOWN_HTTP_RETRIES")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|&value| value <= 8)
        {
            self.http_retries = Some(http_retries);
        }
        if let Ok(value) = env::var("COOLDOWN_VERBOSE") {
            self.verbose = Some(parse_bool(&value));
        }
        if let Ok(value) = env::var("COOLDOWN_SKIP_REGISTRIES") {
            self.skip_registries = parse_registry_skip_list(&value);
        }
    }

    fn finish(self) -> Config {
        Config {
            cooldown_minutes: self.cooldown_minutes.unwrap_or(0),
            mode: self.mode.unwrap_or(Mode::Enforce),
            lockfile_policy: self.lockfile_policy.unwrap_or(LockfilePolicy::Changed),
            now_override: self.now_override,
            ttl_seconds: self.ttl_seconds.unwrap_or(86_400),
            cache_dir: self.cache_dir,
            http_retries: self.http_retries.unwrap_or(2),
            verbose: self.verbose.unwrap_or(false),
            skip_registries: self.skip_registries,
            allowlist: self.allowlist,
        }
    }
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

#[derive(Debug, Clone)]
struct FileConfig {
    path: PathBuf,
    data: CooldownFile,
}

impl FileConfig {
    fn base_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }

    fn resolve_path(&self, candidate: &Path) -> PathBuf {
        if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.base_dir().join(candidate)
        }
    }
}

fn merge_registry_skip_lists(base: &[String], overlay: &[String]) -> Vec<String> {
    let mut merged = base.to_vec();
    for entry in overlay {
        if merged.iter().any(|existing| existing == entry) {
            continue;
        }
        merged.push(entry.clone());
    }
    merged
}

fn parse_registry_skip_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
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

fn read_file_config(path: &Path) -> Option<FileConfig> {
    if !path.exists() {
        return None;
    }

    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) => {
            eprintln!("Failed to read {}: {err}", path.display());
            return None;
        }
    };

    match toml::from_str::<CooldownFile>(&contents) {
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

#[cfg(test)]
mod tests {
    use super::*;

    use assert_fs::TempDir;
    use assert_fs::prelude::*;
    use std::sync::{Mutex, OnceLock};

    use crate::project::{ProjectKind, ProjectMember};

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

    fn project_fixture(root: &Path, member: Option<&Path>) -> ProjectContext {
        ProjectContext {
            cwd: root.to_path_buf(),
            kind: ProjectKind::Workspace,
            workspace_root: root.to_path_buf(),
            members: member
                .map(|path| {
                    vec![ProjectMember {
                        name: "member-a".to_string(),
                        manifest_path: path.join("Cargo.toml"),
                        dir: path.to_path_buf(),
                    }]
                })
                .unwrap_or_default(),
            active_member: member.map(|path| ProjectMember {
                name: "member-a".to_string(),
                manifest_path: path.join("Cargo.toml"),
                dir: path.to_path_buf(),
            }),
        }
    }

    #[test]
    fn skip_registries_support_comma_separated_env() {
        with_env_var(
            "COOLDOWN_SKIP_REGISTRIES",
            Some("crates-io, sparse+https://codeartifact.example/index , mirror"),
            || {
                let root = TempDir::new().unwrap();
                let config = Config::load(&project_fixture(root.path(), None));
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
    fn lockfile_policy_supports_all_env() {
        with_env_var("COOLDOWN_LOCKFILE_POLICY", Some("all"), || {
            let root = TempDir::new().unwrap();
            let config = Config::load(&project_fixture(root.path(), None));
            assert_eq!(config.lockfile_policy, LockfilePolicy::All);
        });
    }

    #[test]
    fn cooldown_now_parses_rfc3339_override() {
        with_env_var("COOLDOWN_NOW", Some("2026-04-03T00:00:00Z"), || {
            let root = TempDir::new().unwrap();
            let config = Config::load(&project_fixture(root.path(), None));
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
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"cooldown_minutes = 15
mode = "warn"
lockfile_policy = "all"
skip_registries = ["crates-io", "mirror"]
verbose = true

[[allow.exact]]
crate = "demo"
version = "1.2.3"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), None));

        assert_eq!(config.cooldown_minutes, 15);
        assert_eq!(config.mode, Mode::Warn);
        assert_eq!(config.lockfile_policy, LockfilePolicy::All);
        assert_eq!(
            config.skip_registries,
            vec!["crates-io".to_string(), "mirror".to_string()]
        );
        assert!(config.verbose);
        assert!(config.allowlist.is_exact_allowed("demo", "1.2.3"));
    }

    #[test]
    fn loads_user_cargo_cooldown_file_when_workspace_missing() {
        let _guard = env_lock().lock().unwrap();

        let root = TempDir::new().unwrap();
        let fake_home = TempDir::new().unwrap();
        fake_home
            .child(".cargo/cooldown.toml")
            .write_str(
                r#"cooldown_minutes = 5
mode = "off"
http_retries = 3
"#,
            )
            .unwrap();
        let original_cargo_home = env::var_os("CARGO_HOME");
        let original_home = env::var("HOME").ok();
        let original_user = env::var("USERPROFILE").ok();

        unsafe { env::remove_var("CARGO_HOME") };
        unsafe { env::set_var("HOME", fake_home.path()) };
        unsafe { env::set_var("USERPROFILE", fake_home.path()) };

        let config = Config::load(&project_fixture(root.path(), None));

        assert_eq!(config.cooldown_minutes, 5);
        assert_eq!(config.mode, Mode::Off);
        assert_eq!(config.http_retries, 3);

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
    }

    #[test]
    fn environment_overrides_file_configuration() {
        let _guard = env_lock().lock().unwrap();

        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"mode = "warn"
lockfile_policy = "all"
skip_registries = ["from-file"]
"#,
            )
            .unwrap();

        let original_mode = env::var("COOLDOWN_MODE").ok();
        let original_lockfile_policy = env::var("COOLDOWN_LOCKFILE_POLICY").ok();
        let original_skips = env::var("COOLDOWN_SKIP_REGISTRIES").ok();

        unsafe { env::set_var("COOLDOWN_MODE", "off") };
        unsafe { env::set_var("COOLDOWN_LOCKFILE_POLICY", "changed") };
        unsafe { env::set_var("COOLDOWN_SKIP_REGISTRIES", "from-env") };

        let config = Config::load(&project_fixture(root.path(), None));
        assert_eq!(config.mode, Mode::Off);
        assert_eq!(config.lockfile_policy, LockfilePolicy::Changed);
        assert_eq!(config.skip_registries, vec!["from-env".to_string()]);

        match original_mode {
            Some(val) => unsafe { env::set_var("COOLDOWN_MODE", val) },
            None => unsafe { env::remove_var("COOLDOWN_MODE") },
        }
        match original_lockfile_policy {
            Some(val) => unsafe { env::set_var("COOLDOWN_LOCKFILE_POLICY", val) },
            None => unsafe { env::remove_var("COOLDOWN_LOCKFILE_POLICY") },
        }
        match original_skips {
            Some(val) => unsafe { env::set_var("COOLDOWN_SKIP_REGISTRIES", val) },
            None => unsafe { env::remove_var("COOLDOWN_SKIP_REGISTRIES") },
        }
    }

    #[test]
    fn uppercase_keys_are_supported_for_backwards_compat() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"COOLDOWN_MINUTES = 9
COOLDOWN_MODE = "warn"
COOLDOWN_LOCKFILE_POLICY = "all"
COOLDOWN_SKIP_REGISTRIES = "crates-io,mirror"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), None));
        assert_eq!(config.cooldown_minutes, 9);
        assert_eq!(config.mode, Mode::Warn);
        assert_eq!(config.lockfile_policy, LockfilePolicy::All);
        assert_eq!(
            config.skip_registries,
            vec!["crates-io".to_string(), "mirror".to_string()]
        );
    }

    #[test]
    fn member_file_overrides_workspace_and_merges_allow_rules() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        let member_dir = root.child("member-a");
        member_dir.create_dir_all().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"cooldown_minutes = 30
skip_registries = ["workspace-registry"]

[[allow.package]]
crate = "serde"
minutes = 20
"#,
            )
            .unwrap();
        member_dir
            .child("cooldown.toml")
            .write_str(
                r#"cooldown_minutes = 5
skip_registries = ["member-registry"]

[allow.global]
minutes = 3

[[allow.package]]
crate = "serde"
minutes = 1

[[allow.exact]]
crate = "foo"
version = "1.2.3"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), Some(member_dir.path())));

        assert_eq!(config.cooldown_minutes, 5);
        assert_eq!(
            config.skip_registries,
            vec![
                "workspace-registry".to_string(),
                "member-registry".to_string(),
            ]
        );
        assert_eq!(config.allowlist.global_minutes(), Some(3));
        assert_eq!(config.allowlist.effective_minutes_for("serde", 90), 1);
        assert!(config.allowlist.is_exact_allowed("foo", "1.2.3"));
    }
}
