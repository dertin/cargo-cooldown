use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tame_index::KrateName;
use tar::Builder;
use tempfile::{TempDir, tempdir};

const CRATE_NAME: &str = "cooldowndep";
const OLD_VERSION: &str = "1.0.0";
const FRESH_VERSION: &str = "1.0.1";
const OLD_PUBTIME: &str = "2026-03-01T00:00:00Z";
const FRESH_PUBTIME: &str = "2026-04-02T12:00:00Z";
const NOW: &str = "2026-04-03T00:00:00Z";
const COOLDOWN_MINUTES: &str = "1440";
const REGISTRY_NAME: &str = "cool-reg";

#[test]
fn uses_index_pubtime_without_hitting_api() {
    let mut harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version(), FRESH_VERSION);

    harness.server.reset_counts();
    let output = harness.run_cooldown(&[("COOLDOWN_VERBOSE", "true")]);
    assert!(
        output.status.success(),
        "cooldown should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);
    assert_eq!(harness.server.api_hits(), 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stderr}");
    assert!(
        stderr.contains("release_time_source=index_pubtime"),
        "expected verbose logs to show local pubtime usage: {}",
        stderr
    );
}

#[test]
fn fills_missing_pubtime_via_fallback_api() {
    let mut harness =
        TestHarness::new(RegistryMode::MissingPubtimeWithApi).expect("harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version(), FRESH_VERSION);

    harness.server.reset_counts();
    let output = harness.run_cooldown(&[("COOLDOWN_VERBOSE", "true")]);
    assert!(
        output.status.success(),
        "cooldown should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);
    assert!(harness.server.api_hits() > 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stderr}");
    assert!(
        stderr.contains("release_time_source=registry_api_fallback"),
        "expected verbose logs to show HTTP fallback usage: {}",
        stderr
    );
}

#[test]
fn fails_closed_when_registry_lacks_release_time_metadata() {
    let mut harness =
        TestHarness::new(RegistryMode::MissingPubtimeNoApi).expect("harness should build");
    harness.generate_lockfile();

    let output = harness.run_cooldown(&[]);
    assert!(!output.status.success(), "cooldown should fail closed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing release timestamp"), "{stderr}");
    assert_eq!(harness.locked_version(), FRESH_VERSION);
}

#[test]
fn warn_mode_continues_when_registry_lacks_release_time_metadata() {
    let mut harness =
        TestHarness::new(RegistryMode::MissingPubtimeNoApi).expect("harness should build");
    harness.generate_lockfile();

    let output = harness.run_cooldown(&[("COOLDOWN_MODE", "warn")]);
    assert!(
        output.status.success(),
        "warn mode should continue: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
}

#[test]
fn skips_registry_from_start_by_name() {
    let mut harness =
        TestHarness::new(RegistryMode::MissingPubtimeNoApi).expect("harness should build");
    harness.generate_lockfile();

    let output = harness.run_cooldown(&[("COOLDOWN_SKIP_REGISTRIES", REGISTRY_NAME)]);
    assert!(
        output.status.success(),
        "skipped registry should be ignored: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
}

#[test]
fn skips_registry_from_start_by_effective_url() {
    let mut harness =
        TestHarness::new(RegistryMode::MissingPubtimeNoApi).expect("harness should build");
    harness.generate_lockfile();
    let skip_value = format!("sparse+{}/index/", harness.server.base_url());

    let output = harness.run_cooldown(&[("COOLDOWN_SKIP_REGISTRIES", &skip_value)]);
    assert!(
        output.status.success(),
        "skipped registry should be ignored: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
}

#[test]
fn mode_off_skips_cooldown_checks_entirely() {
    let mut harness =
        TestHarness::new(RegistryMode::MissingPubtimeNoApi).expect("harness should build");
    harness.generate_lockfile();
    harness.server.reset_counts();

    let output = harness.run_cooldown(&[("COOLDOWN_MODE", "off")]);
    assert!(
        output.status.success(),
        "mode=off should bypass cooldown: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
    assert_eq!(harness.server.api_hits(), 0);
}

#[test]
fn generates_lockfile_before_running_cooldown() {
    let harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    assert!(
        !harness.workspace_dir.join("Cargo.lock").exists(),
        "fixture should start without lockfile"
    );

    let output = harness.run_cooldown(&[]);
    assert!(
        output.status.success(),
        "cooldown should generate a lockfile and continue: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);
}

#[test]
fn honors_manifest_path_in_cargo_style_order_from_external_cwd() {
    let mut harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    harness.generate_lockfile();
    let runner_dir = harness.runner_dir();
    let manifest_path = harness.workspace_dir.join("Cargo.toml");
    let manifest_path = manifest_path.to_string_lossy().to_string();

    let output = harness.run_command_in(
        &runner_dir,
        &["check", "--manifest-path", manifest_path.as_str()],
        &[],
    );
    assert!(
        output.status.success(),
        "manifest-path invocation should succeed from another cwd: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);
}

#[test]
fn honors_manifest_path_before_subcommand_from_external_cwd() {
    let mut harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    harness.generate_lockfile();
    let runner_dir = harness.runner_dir();
    let manifest_path = harness.workspace_dir.join("Cargo.toml");
    let manifest_path = manifest_path.to_string_lossy().to_string();

    let output = harness.run_command_in(
        &runner_dir,
        &["--manifest-path", manifest_path.as_str(), "check"],
        &[],
    );
    assert!(
        output.status.success(),
        "manifest-path before subcommand should still cool the external project: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);
}

#[test]
fn exact_allowlist_keeps_fresh_version_pinned() {
    let mut harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    harness.generate_lockfile();
    let allowlist = harness.workspace_dir.join("cooldown-allowlist.toml");
    fs::write(
        &allowlist,
        format!("[[allow.exact]]\ncrate = \"{CRATE_NAME}\"\nversion = \"{FRESH_VERSION}\"\n"),
    )
    .expect("allowlist should be writable");

    let allowlist_path = allowlist.to_string_lossy().to_string();
    let output = harness.run_cooldown(&[("COOLDOWN_ALLOWLIST_PATH", allowlist_path.as_str())]);
    assert!(
        output.status.success(),
        "exact allowlist should bypass cooldown for the pinned version: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
}

#[test]
fn rejects_update_subcommand_before_running_cooldown() {
    let harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    let output = harness.run_command(&["update"], &[]);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Invoke `cargo update` directly instead"),
        "{stderr}"
    );
}

struct TestHarness {
    _temp_dir: TempDir,
    temp_root: PathBuf,
    cargo_home: PathBuf,
    workspace_dir: PathBuf,
    server: RegistryServer,
}

impl TestHarness {
    fn new(mode: RegistryMode) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let server = RegistryServer::new(mode)?;
        let temp_root = temp_dir.path().to_path_buf();
        let cargo_home = temp_root.join("cargo-home");
        let workspace_dir = temp_root.join("workspace");

        fs::create_dir_all(&cargo_home)?;
        create_workspace(&workspace_dir, &server)?;
        write_registry_config(&cargo_home, &server)?;

        Ok(Self {
            _temp_dir: temp_dir,
            temp_root,
            cargo_home,
            workspace_dir,
            server,
        })
    }

    fn generate_lockfile(&mut self) {
        let output = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .output()
            .expect("cargo generate-lockfile should run");

        assert!(
            output.status.success(),
            "lockfile generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_cooldown(&self, extra_env: &[(&str, &str)]) -> Output {
        self.run_command(&["check"], extra_env)
    }

    fn run_command(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        self.run_command_in(&self.workspace_dir, args, extra_env)
    }

    fn run_command_in(
        &self,
        current_dir: &Path,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"));
        command
            .args(args)
            .current_dir(current_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .env("COOLDOWN_NOW", NOW)
            .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
            .env("COOLDOWN_HTTP_RETRIES", "0");

        for (key, value) in extra_env {
            command.env(key, value);
        }

        command.output().expect("cargo-cooldown should run")
    }

    fn runner_dir(&self) -> PathBuf {
        let path = self.temp_root.join("runner");
        fs::create_dir_all(&path).expect("runner dir should be creatable");
        path
    }

    fn locked_version(&self) -> String {
        let lockfile = fs::read_to_string(self.workspace_dir.join("Cargo.lock"))
            .expect("lockfile should be readable");
        parse_lockfile_version(&lockfile, CRATE_NAME).expect("crate should exist in lockfile")
    }
}

#[derive(Clone, Copy)]
enum RegistryMode {
    PubtimeOnly,
    MissingPubtimeWithApi,
    MissingPubtimeNoApi,
}

#[derive(Clone)]
struct PublishedCrate {
    name: String,
    versions: Vec<PackageVersion>,
}

impl PublishedCrate {
    fn new(name: &str, versions: Vec<PackageVersion>) -> Self {
        Self {
            name: name.to_string(),
            versions,
        }
    }
}

struct RegistryServer {
    base_url: String,
    state: Arc<ServerState>,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl RegistryServer {
    fn new(mode: RegistryMode) -> Result<Self, Box<dyn std::error::Error>> {
        let published_crates = vec![PublishedCrate::new(
            CRATE_NAME,
            vec![
                PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false)?,
                PackageVersion::new(FRESH_VERSION, mode.pubtime_for_fresh(), false)?,
            ],
        )];
        Self::with_crates(published_crates, mode.has_api())
    }

    fn with_crates(
        published_crates: Vec<PublishedCrate>,
        with_api: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let base_paths = build_registry_paths(&base_url, with_api, &published_crates)?;
        let state = Arc::new(ServerState {
            responses: Mutex::new(base_paths),
            request_counts: Mutex::new(HashMap::new()),
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_shutdown = Arc::clone(&shutdown);

        let handle = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let state = Arc::clone(&thread_state);
                        thread::spawn(move || {
                            let _ = handle_stream(stream, state);
                        });
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            base_url,
            state,
            shutdown,
            handle: Some(handle),
        })
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn api_hits(&self) -> usize {
        self.state
            .count_for(&format!("/api/v1/crates/{CRATE_NAME}"))
    }

    fn reset_counts(&self) {
        self.state.request_counts.lock().unwrap().clear();
    }
}

impl Drop for RegistryServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct ServerState {
    responses: Mutex<HashMap<String, ResponseSpec>>,
    request_counts: Mutex<HashMap<String, usize>>,
}

impl ServerState {
    fn count_for(&self, path: &str) -> usize {
        *self.request_counts.lock().unwrap().get(path).unwrap_or(&0)
    }
}

fn handle_stream(mut stream: TcpStream, state: Arc<ServerState>) -> std::io::Result<()> {
    let mut buffer = [0_u8; 4096];
    let bytes = stream.read(&mut buffer)?;
    if bytes == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buffer[..bytes]);
    let mut parts = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    let _method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    *state
        .request_counts
        .lock()
        .unwrap()
        .entry(path.to_string())
        .or_insert(0) += 1;

    let response = state
        .responses
        .lock()
        .unwrap()
        .get(path)
        .cloned()
        .unwrap_or_else(ResponseSpec::not_found);

    write_response(&mut stream, response)
}

#[derive(Clone)]
struct ResponseSpec {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl ResponseSpec {
    fn ok(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            content_type,
            body,
        }
    }

    fn not_found() -> Self {
        Self {
            status: "404 Not Found",
            content_type: "text/plain",
            body: b"not found".to_vec(),
        }
    }
}

fn write_response(stream: &mut TcpStream, response: ResponseSpec) -> std::io::Result<()> {
    let headers = format!(
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.body.len(),
        response.content_type
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

#[derive(Clone)]
struct PackageVersion {
    version: String,
    pubtime: Option<String>,
    yanked: bool,
}

impl PackageVersion {
    fn new(
        version: &str,
        pubtime: Option<&str>,
        yanked: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            version: version.to_string(),
            pubtime: pubtime.map(ToOwned::to_owned),
            yanked,
        })
    }
}

fn create_workspace(
    workspace_dir: &Path,
    server: &RegistryServer,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(workspace_dir.join("src"))?;
    fs::create_dir_all(workspace_dir.join(".cargo"))?;

    fs::write(
        workspace_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "cooldown-workspace"
version = "0.1.0"
edition = "2024"

[dependencies]
{crate_name} = {{ version = "1", registry = "{registry_name}" }}
"#,
            crate_name = CRATE_NAME,
            registry_name = REGISTRY_NAME,
        ),
    )?;
    fs::write(
        workspace_dir.join("src/main.rs"),
        format!(
            r#"fn main() {{
    println!("{{}}", {crate_name}::value());
}}
"#,
            crate_name = CRATE_NAME,
        ),
    )?;
    fs::write(
        workspace_dir.join(".cargo/config.toml"),
        format!(
            r#"[registries.{registry_name}]
index = "sparse+{base_url}/index/"
"#,
            registry_name = REGISTRY_NAME,
            base_url = server.base_url(),
        ),
    )?;

    Ok(())
}

fn write_registry_config(
    cargo_home: &Path,
    server: &RegistryServer,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::write(
        cargo_home.join("config.toml"),
        format!(
            r#"[registries.{registry_name}]
index = "sparse+{base_url}/index/"
"#,
            registry_name = REGISTRY_NAME,
            base_url = server.base_url(),
        ),
    )?;
    Ok(())
}

fn build_tarballs(
    crate_name: &str,
    versions: &[PackageVersion],
) -> Result<HashMap<String, Vec<u8>>, Box<dyn std::error::Error>> {
    let mut tarballs = HashMap::new();
    for version in versions {
        tarballs.insert(
            version.version.clone(),
            create_crate_archive(crate_name, &version.version)?,
        );
    }
    Ok(tarballs)
}

fn create_crate_archive(
    crate_name: &str,
    version: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let package_dir = temp.path().join(format!("{crate_name}-{version}"));
    let root_dir = format!("{crate_name}-{version}");
    fs::create_dir_all(package_dir.join("src"))?;
    fs::write(
        package_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{crate_name}"
version = "{version}"
edition = "2024"

[lib]
path = "src/lib.rs"
"#,
            crate_name = CRATE_NAME,
            version = version,
        ),
    )?;
    fs::write(
        package_dir.join("src/lib.rs"),
        format!(
            r#"pub fn value() -> &'static str {{
    "{version}"
}}
"#,
            version = version,
        ),
    )?;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = Builder::new(&mut encoder);
    builder.append_dir(root_dir.clone(), &package_dir)?;
    builder.append_dir(format!("{root_dir}/src"), package_dir.join("src"))?;
    builder.append_path_with_name(
        package_dir.join("Cargo.toml"),
        format!("{root_dir}/Cargo.toml"),
    )?;
    builder.append_path_with_name(
        package_dir.join("src/lib.rs"),
        format!("{root_dir}/src/lib.rs"),
    )?;
    builder.finish()?;
    drop(builder);

    Ok(encoder.finish()?)
}

fn build_registry_paths(
    base_url: &str,
    with_api: bool,
    published_crates: &[PublishedCrate],
) -> Result<HashMap<String, ResponseSpec>, Box<dyn std::error::Error>> {
    let mut responses = HashMap::new();

    let config_body = if with_api {
        format!(r#"{{"dl":"{base_url}/crates","api":"{base_url}"}}"#)
    } else {
        format!(r#"{{"dl":"{base_url}/crates"}}"#)
    };
    responses.insert(
        "/index/config.json".to_string(),
        ResponseSpec::ok("application/json", config_body.into_bytes()),
    );

    for published in published_crates {
        let krate_name: KrateName<'_> = published.name.as_str().try_into()?;
        let relative_path = krate_name.relative_path(Some('/'));
        let tarballs = build_tarballs(&published.name, &published.versions)?;
        let index_body = build_index_body(&published.name, &published.versions, &tarballs)?;
        responses.insert(
            format!("/index/{relative_path}"),
            ResponseSpec::ok("text/plain", index_body.into_bytes()),
        );

        if with_api {
            responses.insert(
                format!("/api/v1/crates/{}", published.name),
                ResponseSpec::ok(
                    "application/json",
                    build_api_body(&published.versions).into_bytes(),
                ),
            );
        }

        for version in &published.versions {
            responses.insert(
                format!("/crates/{}/{}/download", published.name, version.version),
                ResponseSpec::ok(
                    "application/gzip",
                    tarballs
                        .get(&version.version)
                        .expect("tarball should exist")
                        .clone(),
                ),
            );
        }
    }

    Ok(responses)
}

fn build_index_body(
    crate_name: &str,
    versions: &[PackageVersion],
    tarballs: &HashMap<String, Vec<u8>>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut lines = Vec::new();
    for version in versions {
        let checksum = sha256_hex(
            tarballs
                .get(&version.version)
                .expect("tarball should exist"),
        );
        let mut value = serde_json::json!({
            "name": crate_name,
            "vers": version.version,
            "deps": [],
            "cksum": checksum,
            "features": {},
            "yanked": version.yanked,
        });
        if let Some(pubtime) = &version.pubtime {
            value["pubtime"] = serde_json::Value::String(pubtime.clone());
        }
        lines.push(serde_json::to_string(&value)?);
    }
    Ok(lines.join("\n"))
}

fn build_api_body(versions: &[PackageVersion]) -> String {
    serde_json::json!({
        "versions": versions
            .iter()
            .rev()
            .map(|version| serde_json::json!({
                "num": version.version,
                "created_at": version
                    .pubtime
                    .clone()
                    .unwrap_or_else(|| match version.version.as_str() {
                        OLD_VERSION => OLD_PUBTIME.to_string(),
                        _ => FRESH_PUBTIME.to_string(),
                    }),
                "yanked": version.yanked,
            }))
            .collect::<Vec<_>>(),
    })
    .to_string()
}

fn parse_lockfile_version(lockfile: &str, crate_name: &str) -> Option<String> {
    let mut in_block = false;
    for line in lockfile.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            in_block = false;
            continue;
        }
        if trimmed == format!("name = \"{crate_name}\"") {
            in_block = true;
            continue;
        }
        if in_block && trimmed.starts_with("version = ") {
            return trimmed
                .strip_prefix("version = \"")
                .and_then(|value| value.strip_suffix('"'))
                .map(ToOwned::to_owned);
        }
    }
    None
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl RegistryMode {
    fn pubtime_for_fresh(self) -> Option<&'static str> {
        match self {
            RegistryMode::PubtimeOnly => Some(FRESH_PUBTIME),
            RegistryMode::MissingPubtimeWithApi | RegistryMode::MissingPubtimeNoApi => None,
        }
    }

    fn has_api(self) -> bool {
        matches!(
            self,
            RegistryMode::PubtimeOnly | RegistryMode::MissingPubtimeWithApi
        )
    }
}
