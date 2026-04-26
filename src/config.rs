//! Configuration loading, validation, and merge precedence.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use dirs::home_dir;
use serde::Deserialize;

use crate::allow_rules::{AllowRules, AllowSection};
use crate::project::ProjectContext;

/// Behavior when cooldown cannot fully remove fresh versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    Strict,
    CargoCompatible,
    Off,
}

impl Enforcement {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "strict" => Ok(Enforcement::Strict),
            "cargo_compatible" => Ok(Enforcement::CargoCompatible),
            "off" => Ok(Enforcement::Off),
            _ => {
                bail!(
                    "invalid cooldown enforcement `{value}`; expected one of: strict, cargo_compatible, off"
                )
            }
        }
    }
}

/// Whether cargo-compatible unresolved fresh versions require user confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CargoCompatibleAccept {
    Prompt,
    Auto,
}

impl CargoCompatibleAccept {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "prompt" => Ok(CargoCompatibleAccept::Prompt),
            "auto" => Ok(CargoCompatibleAccept::Auto),
            _ => {
                bail!(
                    "invalid cargo-compatible accept policy `{value}`; expected one of: prompt, auto"
                )
            }
        }
    }
}

/// Whether the initial lockfile is used as a minimum version baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockfileBaselineMode {
    Floor,
    Ignore,
}

impl LockfileBaselineMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "floor" => Ok(LockfileBaselineMode::Floor),
            "ignore" => Ok(LockfileBaselineMode::Ignore),
            _ => bail!("invalid lockfile baseline `{value}`; expected one of: floor, ignore"),
        }
    }

    pub fn uses_initial_lockfile_floor(self) -> bool {
        matches!(self, LockfileBaselineMode::Floor)
    }
}

/// Effective runtime configuration after files and environment are merged.
#[derive(Debug, Clone)]
pub struct Config {
    pub cooldown_minutes: u64,
    pub enforcement: Enforcement,
    pub cargo_compatible_accept: CargoCompatibleAccept,
    pub lockfile_baseline: LockfileBaselineMode,
    pub now_override: Option<DateTime<Utc>>,
    pub ttl_seconds: u64,
    pub cache_dir: Option<PathBuf>,
    pub http_retries: u32,
    pub verbose: bool,
    pub skip_registries: Vec<String>,
    pub allow_rules: AllowRules,
}

impl Config {
    /// Load the effective configuration for one resolved Cargo project.
    ///
    /// Configuration is layered from global Cargo config, workspace
    /// `cooldown.toml`, an active member override when the command targets one
    /// member, and finally environment variables. The returned value is already
    /// validated and contains defaults for any omitted settings.
    pub fn load(project: &ProjectContext) -> Result<Self> {
        let mut merged = MergedConfig::default();

        if let Some(path) = user_config_path() {
            merged.apply_file(&path)?;
        }
        merged.apply_file(&project.workspace_config_path())?;
        if let Some(path) = project.member_config_path() {
            merged.apply_file(&path)?;
        }
        merged.apply_env()?;
        Ok(merged.finish())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
struct CooldownFile {
    cooldown_minutes: Option<u64>,
    enforcement: Option<String>,
    cargo_compatible_accept: Option<String>,
    lockfile_baseline: Option<String>,
    now: Option<String>,
    ttl_seconds: Option<u64>,
    cache_dir: Option<PathBuf>,
    http_retries: Option<u32>,
    verbose: Option<bool>,
    skip_registries: Option<Vec<String>>,
    #[serde(default)]
    allow: AllowSection,
}

#[derive(Debug, Default)]
struct MergedConfig {
    cooldown_minutes: Option<u64>,
    enforcement: Option<Enforcement>,
    cargo_compatible_accept: Option<CargoCompatibleAccept>,
    lockfile_baseline: Option<LockfileBaselineMode>,
    now_override: Option<DateTime<Utc>>,
    ttl_seconds: Option<u64>,
    cache_dir: Option<PathBuf>,
    http_retries: Option<u32>,
    verbose: Option<bool>,
    skip_registries: Vec<String>,
    allow_rules: AllowRules,
}

impl MergedConfig {
    fn apply_file(&mut self, path: &Path) -> Result<()> {
        let Some(file) = read_file_config(path)? else {
            return Ok(());
        };

        if let Some(minutes) = file.data.cooldown_minutes {
            self.cooldown_minutes = Some(minutes);
        }
        if let Some(enforcement) = file.data.enforcement.as_deref() {
            self.enforcement = Some(Enforcement::parse(enforcement)?);
        }
        if let Some(policy) = file.data.cargo_compatible_accept.as_deref() {
            self.cargo_compatible_accept = Some(CargoCompatibleAccept::parse(policy)?);
        }
        if let Some(baseline) = file.data.lockfile_baseline.as_deref() {
            self.lockfile_baseline = Some(LockfileBaselineMode::parse(baseline)?);
        }
        if let Some(now) = file.data.now.as_deref() {
            self.now_override = Some(parse_datetime(now)?);
        }
        if let Some(ttl_seconds) = file.data.ttl_seconds {
            self.ttl_seconds = Some(ttl_seconds);
        }
        if let Some(cache_dir) = file.data.cache_dir.as_ref() {
            self.cache_dir = Some(file.resolve_path(cache_dir));
        }
        if let Some(http_retries) = file.data.http_retries {
            self.http_retries = Some(validate_http_retries(http_retries)?);
        }
        if let Some(verbose) = file.data.verbose {
            self.verbose = Some(verbose);
        }
        if let Some(skip_registries) = file.data.skip_registries.clone() {
            self.skip_registries = merge_registry_skip_lists(
                &self.skip_registries,
                &clean_registry_skip_list(skip_registries),
            );
        }

        self.allow_rules.merge_from(&AllowRules {
            allow: file.data.allow.clone(),
        });
        Ok(())
    }

    fn apply_env(&mut self) -> Result<()> {
        if let Some(minutes) = env_u64("COOLDOWN_MINUTES")? {
            self.cooldown_minutes = Some(minutes);
        }
        if let Ok(value) = env::var("COOLDOWN_ENFORCEMENT") {
            self.enforcement = Some(Enforcement::parse(&value)?);
        }
        if let Ok(value) = env::var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT") {
            self.cargo_compatible_accept = Some(CargoCompatibleAccept::parse(&value)?);
        }
        if let Ok(value) = env::var("COOLDOWN_LOCKFILE_BASELINE") {
            self.lockfile_baseline = Some(LockfileBaselineMode::parse(&value)?);
        }
        if let Ok(value) = env::var("COOLDOWN_NOW") {
            self.now_override = Some(parse_datetime(&value)?);
        }
        if let Some(ttl_seconds) = env_u64("COOLDOWN_TTL_SECONDS")? {
            self.ttl_seconds = Some(ttl_seconds);
        }
        if let Some(cache_dir) = env::var_os("COOLDOWN_CACHE_DIR")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
        {
            self.cache_dir = Some(cache_dir);
        }
        if let Some(http_retries) = env_u32("COOLDOWN_HTTP_RETRIES")? {
            self.http_retries = Some(validate_http_retries(http_retries)?);
        }
        if let Ok(value) = env::var("COOLDOWN_VERBOSE") {
            self.verbose = Some(parse_bool(&value)?);
        }
        if let Ok(value) = env::var("COOLDOWN_SKIP_REGISTRIES") {
            self.skip_registries = parse_registry_skip_list(&value);
        }
        Ok(())
    }

    fn finish(self) -> Config {
        Config {
            cooldown_minutes: self.cooldown_minutes.unwrap_or(0),
            enforcement: self.enforcement.unwrap_or(Enforcement::Strict),
            cargo_compatible_accept: self
                .cargo_compatible_accept
                .unwrap_or(CargoCompatibleAccept::Prompt),
            lockfile_baseline: self
                .lockfile_baseline
                .unwrap_or(LockfileBaselineMode::Floor),
            now_override: self.now_override,
            ttl_seconds: self.ttl_seconds.unwrap_or(86_400),
            cache_dir: self.cache_dir,
            http_retries: self.http_retries.unwrap_or(2),
            verbose: self.verbose.unwrap_or(false),
            skip_registries: self.skip_registries,
            allow_rules: self.allow_rules,
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

fn clean_registry_skip_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn parse_registry_skip_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_bool(value: &str) -> Result<bool> {
    match value {
        "1" => Ok(true),
        "0" => Ok(false),
        _ if value.eq_ignore_ascii_case("true") => Ok(true),
        _ if value.eq_ignore_ascii_case("false") => Ok(false),
        _ => bail!("invalid boolean `{value}`; expected one of: true, false, 1, 0"),
    }
}

fn validate_http_retries(value: u32) -> Result<u32> {
    if value <= 8 {
        Ok(value)
    } else {
        bail!("invalid http_retries `{value}`; expected a value from 0 to 8")
    }
}

fn env_u64(key: &str) -> Result<Option<u64>> {
    let Ok(value) = env::var(key) else {
        return Ok(None);
    };

    value
        .parse()
        .with_context(|| format!("invalid {key} value `{value}`"))
        .map(Some)
}

fn env_u32(key: &str) -> Result<Option<u32>> {
    let Ok(value) = env::var(key) else {
        return Ok(None);
    };

    value
        .parse()
        .with_context(|| format!("invalid {key} value `{value}`"))
        .map(Some)
}

fn parse_datetime(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .with_context(|| format!("invalid RFC 3339 datetime `{value}`"))
}

fn read_file_config(path: &Path) -> Result<Option<FileConfig>> {
    if !path.exists() {
        return Ok(None);
    }

    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let data = toml::from_str::<CooldownFile>(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    Ok(Some(FileConfig {
        path: path.to_path_buf(),
        data,
    }))
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

/// Unit tests for configuration loading, precedence, and validation.
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
            target_directory: root.join("target"),
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
                let config = Config::load(&project_fixture(root.path(), None)).unwrap();
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
    fn lockfile_baseline_supports_ignore_env() {
        with_env_var("COOLDOWN_LOCKFILE_BASELINE", Some("ignore"), || {
            let root = TempDir::new().unwrap();
            let config = Config::load(&project_fixture(root.path(), None)).unwrap();
            assert_eq!(config.lockfile_baseline, LockfileBaselineMode::Ignore);
        });
    }

    #[test]
    fn cooldown_now_parses_rfc3339_override() {
        with_env_var("COOLDOWN_NOW", Some("2026-04-03T00:00:00Z"), || {
            let root = TempDir::new().unwrap();
            let config = Config::load(&project_fixture(root.path(), None)).unwrap();
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
enforcement = "cargo_compatible"
cargo_compatible_accept = "auto"
lockfile_baseline = "ignore"
skip_registries = ["crates-io", "mirror"]
verbose = true

[[allow.exact]]
crate = "demo"
version = "1.2.3"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(config.cooldown_minutes, 15);
        assert_eq!(config.enforcement, Enforcement::CargoCompatible);
        assert_eq!(config.cargo_compatible_accept, CargoCompatibleAccept::Auto);
        assert_eq!(config.lockfile_baseline, LockfileBaselineMode::Ignore);
        assert_eq!(
            config.skip_registries,
            vec!["crates-io".to_string(), "mirror".to_string()]
        );
        assert!(config.verbose);
        assert!(config.allow_rules.is_exact_allowed("demo", "1.2.3"));
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
enforcement = "off"
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

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(config.cooldown_minutes, 5);
        assert_eq!(config.enforcement, Enforcement::Off);
        assert_eq!(
            config.cargo_compatible_accept,
            CargoCompatibleAccept::Prompt
        );
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
                r#"enforcement = "cargo_compatible"
cargo_compatible_accept = "prompt"
lockfile_baseline = "ignore"
skip_registries = ["from-file"]
"#,
            )
            .unwrap();

        let original_enforcement = env::var("COOLDOWN_ENFORCEMENT").ok();
        let original_accept = env::var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT").ok();
        let original_lockfile_baseline = env::var("COOLDOWN_LOCKFILE_BASELINE").ok();
        let original_skips = env::var("COOLDOWN_SKIP_REGISTRIES").ok();

        unsafe { env::set_var("COOLDOWN_ENFORCEMENT", "off") };
        unsafe { env::set_var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT", "auto") };
        unsafe { env::set_var("COOLDOWN_LOCKFILE_BASELINE", "floor") };
        unsafe { env::set_var("COOLDOWN_SKIP_REGISTRIES", "from-env") };

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();
        assert_eq!(config.enforcement, Enforcement::Off);
        assert_eq!(config.cargo_compatible_accept, CargoCompatibleAccept::Auto);
        assert_eq!(config.lockfile_baseline, LockfileBaselineMode::Floor);
        assert_eq!(config.skip_registries, vec!["from-env".to_string()]);

        match original_enforcement {
            Some(val) => unsafe { env::set_var("COOLDOWN_ENFORCEMENT", val) },
            None => unsafe { env::remove_var("COOLDOWN_ENFORCEMENT") },
        }
        match original_accept {
            Some(val) => unsafe { env::set_var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT", val) },
            None => unsafe { env::remove_var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT") },
        }
        match original_lockfile_baseline {
            Some(val) => unsafe { env::set_var("COOLDOWN_LOCKFILE_BASELINE", val) },
            None => unsafe { env::remove_var("COOLDOWN_LOCKFILE_BASELINE") },
        }
        match original_skips {
            Some(val) => unsafe { env::set_var("COOLDOWN_SKIP_REGISTRIES", val) },
            None => unsafe { env::remove_var("COOLDOWN_SKIP_REGISTRIES") },
        }
    }

    #[test]
    fn rejects_unknown_enforcement_value() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(r#"enforcement = "soft""#)
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid cooldown enforcement `soft`"),
            "{err:#}"
        );
    }

    #[test]
    fn rejects_unknown_enforcement_value_from_env() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        let original_enforcement = env::var("COOLDOWN_ENFORCEMENT").ok();
        unsafe { env::set_var("COOLDOWN_ENFORCEMENT", "soft") };

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid cooldown enforcement `soft`"),
            "{err:#}"
        );

        match original_enforcement {
            Some(val) => unsafe { env::set_var("COOLDOWN_ENFORCEMENT", val) },
            None => unsafe { env::remove_var("COOLDOWN_ENFORCEMENT") },
        }
    }

    #[test]
    fn rejects_unknown_cargo_compatible_accept_value() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(r#"cargo_compatible_accept = "always""#)
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid cargo-compatible accept policy `always`"),
            "{err:#}"
        );
    }

    #[test]
    fn rejects_unknown_lockfile_baseline_value() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(r#"lockfile_baseline = "everything""#)
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid lockfile baseline `everything`"),
            "{err:#}"
        );
    }

    #[test]
    fn rejects_unknown_file_keys() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(r#"cooldown_seconds = 9"#)
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(format!("{err:#}").contains("unknown field"), "{err:#}");
    }

    #[test]
    fn rejects_unknown_allow_rule_keys() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[[allow.package]]
crate = "serde"
seconds = 60
"#,
            )
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(format!("{err:#}").contains("unknown field"), "{err:#}");
    }

    #[test]
    fn rejects_invalid_verbose_env_value() {
        with_env_var("COOLDOWN_VERBOSE", Some("yes"), || {
            let root = TempDir::new().unwrap();
            let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
            assert!(
                format!("{err:#}").contains("invalid boolean `yes`"),
                "{err:#}"
            );
        });
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

        let config = Config::load(&project_fixture(root.path(), Some(member_dir.path()))).unwrap();

        assert_eq!(config.cooldown_minutes, 5);
        assert_eq!(
            config.skip_registries,
            vec![
                "workspace-registry".to_string(),
                "member-registry".to_string(),
            ]
        );
        assert_eq!(config.allow_rules.global_minutes(), Some(3));
        assert_eq!(config.allow_rules.effective_minutes_for("serde", 90), 1);
        assert!(config.allow_rules.is_exact_allowed("foo", "1.2.3"));
    }
}
