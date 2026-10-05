//! Process-owned sparse index. Allowed JSON lines are forwarded byte-for-byte.
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

#[derive(Debug)]
pub struct Unsupported(pub String);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; use legacy", self.0)
    }
}
impl std::error::Error for Unsupported {}

use chrono::{DateTime, Utc};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::Digest;

use crate::backend::Backend;
use crate::config::{Config, IncompatiblePublishAgePolicy, LockfileBaselineMode};
use crate::isolation::IsolatedWorkspace;
use crate::project::ProjectContext;
use crate::registry::{RegistryContext, resolve_registry_context};

const CRATES_IO: &str = "registry+https://github.com/rust-lang/crates.io-index";

/// Unsupported cases are decided before starting speculative resolution.
pub fn support(project: &ProjectContext, args: &[OsString]) -> Result<()> {
    let command = args.first().and_then(|arg| arg.to_str()).unwrap_or("");
    ensure!(
        matches!(
            command,
            "update" | "generate-lockfile" | "check" | "build" | "test" | "run"
        ),
        "command requires legacy"
    );
    for arg in args.iter().take_while(|arg| *arg != "--") {
        let arg = arg.to_string_lossy();
        ensure!(
            !matches!(arg.as_ref(), "--offline" | "--frozen" | "--locked")
                && !arg.starts_with("--config")
                && !arg.starts_with("-Z")
                && !arg.starts_with("--precise"),
            "offline, frozen, locked, precise and Cargo overrides require legacy"
        );
    }
    ensure!(
        std::env::var("CARGO_NET_OFFLINE").as_deref() != Ok("true"),
        "offline requires legacy"
    );
    if !matches!(command, "update" | "generate-lockfile") {
        ensure!(
            !project.workspace_root.join("Cargo.lock").exists(),
            "build with existing lockfile uses legacy graph validation"
        );
    }
    let cargo_config = crate::project::cargo_config(&project.cwd)?;
    ensure!(
        cargo_config.get("source").is_none(),
        "source replacement, Git indexes and vendor require legacy"
    );
    ensure!(
        cargo_config
            .get("net")
            .and_then(|v| v.get("offline"))
            .and_then(|v| v.as_bool())
            != Some(true),
        "offline requires legacy"
    );
    if let Some(registries) = cargo_config.get("registries").and_then(|v| v.as_table()) {
        for entry in registries.values() {
            ensure!(
                entry.get("credential-provider").is_none() && entry.get("token").is_none(),
                "private registries require legacy"
            );
            if let Some(index) = entry.get("index").and_then(|v| v.as_str()) {
                ensure!(index.starts_with("sparse+"), "Git indexes require legacy");
            }
        }
    }
    for key in std::env::vars_os().map(|(key, _)| key) {
        let key = key.to_string_lossy();
        ensure!(
            !(key.starts_with("CARGO_REGISTRIES_") && key.ends_with("_TOKEN"))
                && key != "CARGO_REGISTRY_TOKEN",
            "private registries require legacy"
        );
    }
    Ok(())
}

pub fn native_config_compatible(project: &ProjectContext) -> Result<bool> {
    let config = crate::project::cargo_config(&project.cwd)?;
    // Native per-registry values override the translated global setting.
    Ok(!config.to_string().contains("min-publish-age"))
}

fn cargo_http_proxy(project: &ProjectContext) -> Result<Option<String>> {
    match std::env::var("CARGO_HTTP_PROXY") {
        Ok(proxy) => return Ok(Some(proxy)),
        Err(std::env::VarError::NotPresent) => {}
        Err(err) => return Err(err).context("invalid CARGO_HTTP_PROXY"),
    }
    let config = crate::project::cargo_config(&project.cwd)?;
    config
        .get("http")
        .and_then(|http| http.get("proxy"))
        .map(|proxy| {
            proxy
                .as_str()
                .map(str::to_owned)
                .context("http.proxy must be a string")
        })
        .transpose()
}

fn http_client(proxy: Option<&str>) -> Result<Client> {
    let mut builder = Client::builder().timeout(Duration::from_secs(15));
    if let Some(proxy) = proxy {
        builder = builder.no_proxy();
        if !proxy.is_empty() {
            let proxy = reqwest::Proxy::all(proxy)?.no_proxy(reqwest::NoProxy::from_env());
            builder = builder.proxy(proxy);
        }
    }
    Ok(builder.build()?)
}

struct Registry {
    source: String,
    context: RegistryContext,
    upstream: String,
    baseline_versions: HashMap<String, HashSet<String>>,
}

fn registries(config: &Config, project: &ProjectContext) -> Result<Vec<Registry>> {
    let table = crate::project::cargo_config(&project.cwd)?;
    let mut sources = vec![CRATES_IO.to_string()];
    if let Some(registries) = table.get("registries").and_then(|v| v.as_table()) {
        for entry in registries.values() {
            if let Some(index) = entry.get("index").and_then(|v| v.as_str()) {
                sources.push(index.to_string());
            }
            ensure!(
                entry.get("credential-provider").is_none() && entry.get("token").is_none(),
                "private registries require legacy"
            );
        }
    }
    for (key, value) in std::env::vars() {
        if key.starts_with("CARGO_REGISTRIES_") && key.ends_with("_INDEX") {
            sources.push(value);
        }
    }
    sources.sort();
    sources.dedup();
    let mut result: Vec<Registry> = Vec::new();
    for source in sources {
        let context = resolve_registry_context(&source, &config.skip_registries)?;
        if result
            .iter()
            .any(|registry| registry.context.effective_index_url == context.effective_index_url)
        {
            continue;
        }
        let upstream = context
            .effective_index_url
            .strip_prefix("sparse+")
            .ok_or_else(|| Unsupported("Git indexes".to_string()))?
            .to_string();
        let url = reqwest::Url::parse(&upstream)?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none(),
            "private registries require legacy"
        );
        result.push(Registry {
            source,
            context,
            upstream,
            baseline_versions: HashMap::new(),
        });
    }
    Ok(result)
}

#[derive(Default, Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<Package>,
}
#[derive(Deserialize)]
struct Package {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
}
fn read_lockfile(path: &Path) -> Result<Lockfile> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Lockfile::default()),
        Err(err) => Err(err.into()),
    }
}
fn source_matches(source: &str, registry: &Registry) -> bool {
    let source_url = source
        .trim_start_matches("registry+")
        .trim_start_matches("sparse+");
    let original_url = registry
        .source
        .trim_start_matches("registry+")
        .trim_start_matches("sparse+");
    source_url == original_url || source_url == registry.upstream
}

#[derive(Deserialize)]
struct IndexEntry {
    name: String,
    vers: String,
    cksum: String,
    pubtime: Option<DateTime<Utc>>,
    #[serde(default)]
    deps: Vec<IndexDependency>,
}
#[derive(Deserialize)]
struct IndexDependency {
    registry: Option<String>,
}
#[derive(Deserialize)]
struct IndexConfig {
    dl: String,
    api: Option<String>,
    #[serde(default, rename = "auth-required")]
    auth_required: bool,
}
#[derive(Serialize, Deserialize)]
struct Original {
    url: String,
    etag: Option<String>,
    modified: Option<String>,
    body: String,
}
struct FilteredCrate {
    body: String,
    checksums: HashMap<String, String>,
}
// A cell coalesces concurrent requests for one crate without serializing other crates.
type CrateCell = Arc<Mutex<Option<Arc<FilteredCrate>>>>;
struct State {
    config: Config,
    registries: Vec<Registry>,
    baseline: Lockfile,
    now: DateTime<Utc>,
    per_crate: HashMap<String, u64>,
    client: Mutex<Option<Client>>,
    http_proxy: Option<String>,
    cache: PathBuf,
    crates: Mutex<HashMap<(usize, String), CrateCell>>,
    originals: Mutex<HashMap<(usize, String), String>>,
    requests: AtomicU64,
    bytes: AtomicU64,
    unsupported: Mutex<Option<String>>,
    request_failure: Mutex<Option<String>>,
    excluded_versions: AtomicBool,
}
impl State {
    fn http_client(&self) -> Result<Client> {
        let mut client = self
            .client
            .lock()
            .map_err(|_| anyhow::anyhow!("HTTP client lock poisoned"))?;
        if let Some(client) = client.as_ref() {
            return Ok(client.clone());
        }
        let initialized = http_client(self.http_proxy.as_deref())?;
        *client = Some(initialized.clone());
        Ok(initialized)
    }

    fn original(&self, registry: usize, path: &str) -> Result<String> {
        let key = (registry, path.to_string());
        if let Some(body) = self
            .originals
            .lock()
            .map_err(|_| anyhow::anyhow!("original cache poisoned"))?
            .get(&key)
        {
            return Ok(body.clone());
        }
        let body = self.fetch_original(registry, path)?;
        self.originals
            .lock()
            .map_err(|_| anyhow::anyhow!("original cache poisoned"))?
            .insert(key, body.clone());
        Ok(body)
    }

    fn fetch_original(&self, registry: usize, path: &str) -> Result<String> {
        let url = format!("{}{path}", self.registries[registry].upstream);
        let cache_path = self.cache.join(format!("{:016x}", fingerprint(&url)));
        let previous: Option<Original> = fs::read(&cache_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .filter(|original: &Original| original.url == url);
        let mut request = self.http_client()?.get(&url);
        if let Some(original) = &previous {
            if let Some(etag) = &original.etag {
                request = request.header("If-None-Match", etag);
            }
            if let Some(modified) = &original.modified {
                request = request.header("If-Modified-Since", modified);
            }
        }
        let mut attempt = 0;
        let response = loop {
            self.requests.fetch_add(1, Ordering::Relaxed);
            let response = request
                .try_clone()
                .context("could not clone index GET")?
                .send()
                .and_then(|response| response.error_for_status());
            match response {
                Ok(response) => break response,
                Err(err)
                    if attempt < self.config.http_retries
                        && (err.is_timeout()
                            || err.is_connect()
                            || err.status().is_some_and(|status| {
                                status.is_server_error() || status.as_u16() == 429
                            })) =>
                {
                    attempt += 1;
                    thread::sleep(Duration::from_millis(50 * u64::from(attempt)));
                }
                Err(err) => return Err(err.into()),
            }
        };
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(previous.context("304 without original index")?.body);
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let modified = response
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response.text()?;
        self.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
        fs::create_dir_all(&self.cache)?;
        let original = Original {
            url,
            etag,
            modified,
            body,
        };
        let mut temp = tempfile::NamedTempFile::new_in(&self.cache)?;
        {
            let mut writer = BufWriter::new(&mut temp);
            serde_json::to_writer(&mut writer, &original)?;
            writer.flush()?;
        }
        temp.persist(cache_path)?;
        Ok(original.body)
    }

    fn crate_data(&self, registry: usize, name: &str) -> Result<Arc<FilteredCrate>> {
        let cell = {
            let mut crates = self
                .crates
                .lock()
                .map_err(|_| anyhow::anyhow!("index cache poisoned"))?;
            Arc::clone(
                crates
                    .entry((registry, name.to_string()))
                    .or_insert_with(|| Arc::new(Mutex::new(None))),
            )
        };
        let mut cached = cell
            .lock()
            .map_err(|_| anyhow::anyhow!("crate cache poisoned"))?;
        if let Some(data) = &*cached {
            return Ok(Arc::clone(data));
        }
        let path = crate_path(name)?;
        let index_config: IndexConfig =
            serde_json::from_str(&self.original(registry, "config.json")?)?;
        if index_config.auth_required {
            return Err(Unsupported("private registry".to_string()).into());
        }
        let original = self.original(registry, &path)?;
        let mut result = FilteredCrate {
            body: String::new(),
            checksums: HashMap::new(),
        };
        let context = &self.registries[registry].context;
        // Compute the registry default once. Package rules were normalized at startup.
        let default_seconds = self.config.min_publish_age_seconds_for(context, "")?;
        let seconds = self
            .per_crate
            .get(name)
            .copied()
            .unwrap_or(default_seconds)
            .min(default_seconds);
        for line in original.lines().filter(|line| !line.is_empty()) {
            let entry: IndexEntry = serde_json::from_str(line)?;
            ensure!(
                entry.name.eq_ignore_ascii_case(name),
                "index returned a different crate"
            );
            for dependency in &entry.deps {
                if let Some(source) = &dependency.registry
                    && !self.registries.iter().any(|r| source_matches(source, r))
                {
                    return Err(Unsupported(
                        "cross-registry dependency is not configured".to_string(),
                    )
                    .into());
                }
            }
            let baseline = self.registries[registry]
                .baseline_versions
                .get(name)
                .is_some_and(|versions| versions.contains(&entry.vers));
            let exempt = baseline
                || context.skipped
                || seconds == 0
                || self.config.incompatible_publish_age == IncompatiblePublishAgePolicy::Allow
                || self.config.allow_rules.is_exact_allowed(name, &entry.vers);
            let allowed = if exempt {
                true
            } else {
                let published = entry.pubtime.ok_or_else(|| {
                    Unsupported(format!("missing pubtime for {name}@{}", entry.vers))
                })?;
                let age = self.now.signed_duration_since(published).num_seconds();
                age >= 0 && age as u64 >= seconds
            };
            if allowed {
                result.body.push_str(line);
                result.body.push('\n');
                result.checksums.insert(entry.vers, entry.cksum);
            } else {
                self.excluded_versions.store(true, Ordering::Relaxed);
            }
        }
        let result = Arc::new(result);
        *cached = Some(Arc::clone(&result));
        Ok(result)
    }

    fn response(&self, path: &str) -> Result<String> {
        let (registry, resource) = path
            .trim_start_matches('/')
            .split_once('/')
            .context("invalid sparse path")?;
        let registry: usize = registry.parse()?;
        ensure!(registry < self.registries.len(), "unknown registry");
        if resource == "config.json" {
            let body = self.original(registry, "config.json")?;
            let config: IndexConfig = serde_json::from_str(&body)?;
            if config.auth_required {
                return Err(Unsupported("private registry".to_string()).into());
            }
            ensure!(!config.dl.is_empty(), "registry has no download endpoint");
            let _ = config.api;
            return Ok(body);
        }
        let name = resource.rsplit('/').next().context("missing crate name")?;
        ensure!(crate_path(name)? == resource, "invalid crate path");
        Ok(self.crate_data(registry, name)?.body.clone())
    }

    fn load_baseline_indexes(&self, packages: &[(usize, &Package)]) -> Result<()> {
        if packages.len() <= 1 {
            for (registry, package) in packages {
                self.crate_data(*registry, &package.name)?;
            }
            return Ok(());
        }
        // Load each registry config before concurrent crate requests. All these
        // indexes are required by targeted baseline-ignore validation.
        let registries: HashSet<usize> = packages.iter().map(|(index, _)| *index).collect();
        for registry in registries {
            self.original(registry, "config.json")?;
        }
        thread::scope(|scope| -> Result<()> {
            let mut workers = Vec::new();
            for chunk in packages.chunks(packages.len().div_ceil(8)) {
                workers.push(thread::Builder::new().spawn_scoped(
                    scope,
                    move || -> Result<()> {
                        for (registry, package) in chunk {
                            self.crate_data(*registry, &package.name)?;
                        }
                        Ok(())
                    },
                )?);
            }
            // Join every worker before propagating errors or releasing isolation.
            let mut result = Ok(());
            for worker in workers {
                let completed = worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("baseline index worker panicked"))
                    .and_then(|result| result);
                if result.is_ok() {
                    result = completed;
                }
            }
            result
        })
    }

    fn seed_native_cache(&self, candidate: &Lockfile) -> Result<()> {
        let Ok(options) = tame_index::utils::flock::LockOptions::cargo_package_lock(None) else {
            return Ok(());
        };
        let Ok(lock) = options.shared().try_lock() else {
            return Ok(());
        };
        let mut originals = self
            .originals
            .lock()
            .map_err(|_| anyhow::anyhow!("original cache poisoned"))?;
        for (index, registry) in self.registries.iter().enumerate() {
            if let Ok(Some(body)) = crate::registry::cached_sparse_config(&registry.context) {
                originals.insert((index, "config.json".to_string()), body);
            }
        }
        for package in &candidate.package {
            let Some(source) = &package.source else {
                continue;
            };
            let Some(index) = self
                .registries
                .iter()
                .position(|registry| source_matches(source, registry))
            else {
                continue;
            };
            if let Ok(Some(body)) = crate::registry::cached_sparse_crate(
                &self.registries[index].context,
                &package.name,
                &lock,
            ) {
                originals.insert((index, crate_path(&package.name)?), body);
            }
        }
        Ok(())
    }

    fn validate(&self, path: &Path, backend: Backend) -> Result<()> {
        ensure!(path.is_file(), "Cargo did not produce a lockfile");
        let candidate = read_lockfile(path)?;
        if backend == Backend::Native {
            // Cargo just refreshed these entries during resolution. Reuse their
            // original metadata, then apply the same age and checksum checks.
            // Missing or unreadable entries fall back to HTTP after unlocking.
            self.seed_native_cache(&candidate)?;
        }
        for package in candidate.package {
            let Some(source) = package.source.as_deref() else {
                continue;
            };
            if source.starts_with("git+") {
                return Err(Unsupported("Git dependencies".to_string()).into());
            }
            let registry = self
                .registries
                .iter()
                .position(|r| source_matches(source, r))
                .context("candidate contains an unfiltered registry")?;
            let data = self.crate_data(registry, &package.name)?;
            ensure!(
                data.checksums.get(&package.version) == package.checksum.as_ref()
                    && package.checksum.is_some(),
                "candidate violates cooldown or checksum for {}@{}",
                package.name,
                package.version
            );
        }
        Ok(())
    }
}

fn crate_path(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')),
        "invalid crate name"
    );
    let name = name.to_ascii_lowercase();
    Ok(match name.len() {
        1 => format!("1/{name}"),
        2 => format!("2/{name}"),
        3 => format!("3/{}/{name}", &name[..1]),
        _ => format!("{}/{}/{name}", &name[..2], &name[2..4]),
    })
}
fn fingerprint(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

struct Server {
    address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}
impl Server {
    fn start(state: Arc<State>, project: &ProjectContext) -> Result<Self> {
        let identity = format!(
            "{}:{:?}",
            project.workspace_root.display(),
            std::env::var_os("CARGO_HOME")
        );
        let port = 20000 + (fingerprint(&identity) % 40000) as u16;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .or_else(|_| TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut server = Self {
            address,
            stop,
            workers: Vec::new(),
        };
        for _ in 0..8 {
            let listener = listener.try_clone()?;
            let stop = Arc::clone(&server.stop);
            let state = Arc::clone(&state);
            server.workers.push(thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if let Err(err) = serve(stream, &state, &stop) {
                                tracing::debug!(error = %err, "sparse request failed");
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1))
                        }
                        Err(_) => break,
                    }
                }
            }));
        }
        Ok(server)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        tracing::debug!("stopping sparse index workers");
        self.stop.store(true, Ordering::Relaxed);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        tracing::debug!("sparse index workers stopped");
    }
}
// Poll cancellation without treating Cargo's pauses or split headers as EOF.
fn read_http_line(
    reader: &mut BufReader<TcpStream>,
    line: &mut String,
    stop: &AtomicBool,
    deadline: Instant,
) -> Result<usize> {
    use std::io::Read;
    while !stop.load(Ordering::Relaxed) {
        ensure!(Instant::now() < deadline, "HTTP request timed out");
        ensure!(line.len() < 8192, "HTTP line too large");
        let remaining = (8192 - line.len()) as u64;
        match reader.by_ref().take(remaining).read_line(line) {
            Ok(0) if line.is_empty() => return Ok(0),
            Ok(_) if line.ends_with('\n') => return Ok(line.len()),
            Ok(_) => bail!("incomplete HTTP line"),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(0)
}

fn serve(stream: TcpStream, state: &State, stop: &AtomicBool) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream);
    // Cargo can reuse each connection for independent index requests.
    while !stop.load(Ordering::Relaxed) {
        let mut first = String::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        if read_http_line(&mut reader, &mut first, stop, deadline)? == 0 {
            break;
        }
        let mut header_bytes = first.len();
        let mut previous_etag = None;
        loop {
            let mut header = String::new();
            let read = read_http_line(&mut reader, &mut header, stop, deadline)?;
            header_bytes += read;
            ensure!(header_bytes <= 8192, "HTTP headers too large");
            if read == 0 {
                return Ok(());
            }
            if header == "\r\n" {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("if-none-match")
            {
                previous_etag = Some(value.trim().to_string());
            }
        }
        let fields: Vec<&str> = first.split_whitespace().collect();
        ensure!(
            fields.len() == 3 && fields[0] == "GET",
            "invalid HTTP request"
        );
        let (status, body) = match state.response(fields[1]) {
            Ok(body) => ("200 OK", body),
            Err(err) => {
                let mut status = "404 Not Found";
                if let Some(reason) = err.downcast_ref::<Unsupported>()
                    && let Ok(mut slot) = state.unsupported.lock()
                {
                    *slot = Some(reason.0.clone());
                } else if err
                    .downcast_ref::<reqwest::Error>()
                    .and_then(reqwest::Error::status)
                    != Some(reqwest::StatusCode::NOT_FOUND)
                {
                    // Cargo may backtrack successfully after a missing crate.
                    // An unavailable or invalid index is not evidence of absence.
                    status = "502 Bad Gateway";
                    if let Ok(mut slot) = state.request_failure.lock() {
                        *slot = Some(format!("{err:#}"));
                    }
                }
                tracing::warn!(error = %err, "filtered index rejected request");
                (status, format!("{err:#}"))
            }
        };
        let digest = sha2::Sha256::digest(body.as_bytes());
        let mut etag = String::with_capacity(66);
        etag.push('"');
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in digest {
            etag.push(char::from(HEX[usize::from(byte >> 4)]));
            etag.push(char::from(HEX[usize::from(byte & 15)]));
        }
        etag.push('"');
        if status == "200 OK" && previous_etag.as_deref() == Some(etag.as_str()) {
            write!(
                reader.get_mut(),
                "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nCache-Control: no-cache\r\n\r\n"
            )?;
            continue;
        }
        write!(
            reader.get_mut(),
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nETag: {etag}\r\nCache-Control: no-cache\r\n\r\n{body}",
            body.len()
        )?;
    }
    Ok(())
}

pub fn run(
    config: &Config,
    project: &ProjectContext,
    cli: &crate::Cli,
    args: &[OsString],
    backend: Backend,
) -> Result<i32> {
    let start = Instant::now();
    let isolated = IsolatedWorkspace::create(project, &cli.manifest)?;
    let isolation_ms = start.elapsed().as_millis();
    let baseline = read_lockfile(isolated.lockfile_path())?;
    // Selection ran before coordination. Another writer may have published a
    // baseline while we waited; native full updates cannot preserve its floor.
    if backend == Backend::Native
        && !baseline.package.is_empty()
        && !(config.lockfile_baseline == LockfileBaselineMode::Ignore
            && args.first().is_some_and(|arg| arg == "generate-lockfile"))
    {
        return Err(Unsupported(
            "native backend observed an existing baseline after acquiring coordination".to_string(),
        )
        .into());
    }
    let preparation = Instant::now();
    let is_resolution = args
        .first()
        .is_some_and(|arg| arg == "update" || arg == "generate-lockfile");
    let dry_run = crate::is_dry_run_request(args);
    let resolution_args = if is_resolution {
        crate::without_dry_run_flag(args)
    } else {
        vec![OsString::from("generate-lockfile")]
    };
    let resolution_args = isolated.rewrite_cargo_args(&resolution_args);
    let mut command = if backend == Backend::Native {
        crate::backend::cargo()
    } else {
        crate::backend::own_cargo()
    };
    command
        .current_dir(isolated.current_dir())
        .args(resolution_args);
    let mut server = None;
    let registry_started = Instant::now();
    let mut registries = registries(config, project)?;
    if config.lockfile_baseline.uses_initial_lockfile_floor() {
        for registry in &mut registries {
            for package in &baseline.package {
                if package
                    .source
                    .as_deref()
                    .is_some_and(|source| source_matches(source, registry))
                {
                    registry
                        .baseline_versions
                        .entry(package.name.clone())
                        .or_default()
                        .insert(package.version.clone());
                }
            }
        }
    }
    let registry_setup_ms = registry_started.elapsed().as_millis();
    if config.verbose {
        eprintln!("cooldown: registry_setup_ms={registry_setup_ms}");
    }
    let cache = config
        .cache_dir
        .clone()
        .unwrap_or_else(|| {
            dirs::cache_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("cargo-cooldown")
        })
        .join("sparse-originals-v1");
    let http_proxy = cargo_http_proxy(project)?;
    let client = if backend == Backend::Native {
        None
    } else {
        Some(http_client(http_proxy.as_deref())?)
    };
    let state = Arc::new(State {
        config: config.clone(),
        registries,
        baseline,
        now: config.now_override.unwrap_or_else(Utc::now),
        per_crate: config.allow_rules.per_crate_min_publish_age_seconds(),
        client: Mutex::new(client),
        http_proxy,
        cache,
        originals: Mutex::new(HashMap::new()),
        crates: Mutex::new(HashMap::new()),
        requests: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        unsupported: Mutex::new(None),
        request_failure: Mutex::new(None),
        excluded_versions: AtomicBool::new(false),
    });
    if backend == Backend::Filtered {
        command.env("CARGO_HTTP_PROXY", "");
        // Legacy baseline=ignore cools the whole graph even for targeted updates.
        // Unlock only additional fresh baseline packages, keeping unrelated aged
        // packages at Cargo's targeted-update scope and retaining one resolution.
        if args.first().is_some_and(|arg| arg == "update")
            && !cli.workspace.package.is_empty()
            && !config.lockfile_baseline.uses_initial_lockfile_floor()
        {
            let mut packages = Vec::new();
            for package in &state.baseline.package {
                let Some(source) = &package.source else {
                    continue;
                };
                let Some(registry) = state
                    .registries
                    .iter()
                    .position(|registry| source_matches(source, registry))
                else {
                    continue;
                };
                let explicitly_selected = cli.workspace.package.iter().any(|spec| {
                    let name = spec
                        .rsplit('#')
                        .next()
                        .unwrap_or(spec)
                        .split(['@', ':'])
                        .next();
                    name == Some(package.name.as_str())
                });
                if !explicitly_selected {
                    packages.push((registry, package));
                }
            }
            state.load_baseline_indexes(&packages)?;
            for (registry, package) in packages {
                if !state
                    .crate_data(registry, &package.name)?
                    .checksums
                    .contains_key(&package.version)
                {
                    if args
                        .iter()
                        .any(|arg| arg.to_string_lossy().starts_with("--precise"))
                        || state
                            .baseline
                            .package
                            .iter()
                            .filter(|other| {
                                other.name == package.name && other.version == package.version
                            })
                            .count()
                            > 1
                    {
                        return Err(Unsupported("targeted baseline-ignore update requires additional ambiguous or precise targets".to_string()).into());
                    }
                    command.args([
                        "--package",
                        &format!("{}@{}", package.name, package.version),
                    ]);
                }
            }
        }
        let running = Server::start(Arc::clone(&state), project)?;
        for (index, registry) in state.registries.iter().enumerate() {
            let original_name = if registry.source == CRATES_IO {
                "crates-io".to_string()
            } else {
                format!("cooldown-original-{index}")
            };
            if registry.source != CRATES_IO {
                command.args([
                    "--config",
                    &format!(
                        "source.{original_name}.registry={:?}",
                        registry.context.effective_index_url
                    ),
                ]);
            }
            command.args([
                "--config",
                &format!("source.{original_name}.replace-with=\"cooldown-filter-{index}\""),
            ]);
            command.args([
                "--config",
                &format!(
                    "source.cooldown-filter-{index}.registry=\"sparse+http://{}/{index}/\"",
                    running.address
                ),
            ]);
        }
        server = Some(running);
    } else {
        command.args([
            "--config",
            &format!(
                "registry.global-min-publish-age=\"{} seconds\"",
                config.min_publish_age_seconds
            ),
            "--config",
            "resolver.incompatible-publish-age=\"deny\"",
        ]);
        command.env("CARGO_RESOLVER_INCOMPATIBLE_PUBLISH_AGE", "deny");
    }
    let prepare_ms = preparation.elapsed().as_millis();
    let resolution = Instant::now();
    tracing::debug!("starting sparse Cargo resolution");
    let output = command.output()?;
    tracing::debug!(
        success = output.status.success(),
        "sparse Cargo resolution finished"
    );
    if let Some(reason) = state
        .request_failure
        .lock()
        .map_err(|_| anyhow::anyhow!("server state poisoned"))?
        .as_ref()
    {
        bail!("filtered index request failed: {reason}");
    }
    if let Some(reason) = state
        .unsupported
        .lock()
        .map_err(|_| anyhow::anyhow!("server state poisoned"))?
        .clone()
    {
        return Err(Unsupported(reason).into());
    }
    if !output.status.success() {
        if config.incompatible_publish_age == IncompatiblePublishAgePolicy::Fallback
            && state.excluded_versions.load(Ordering::Relaxed)
        {
            // Network/metadata failures were handled above. Legacy must start
            // from the original baseline, with no proxy workers still active.
            if config.backend == Backend::Auto && args.first().is_some_and(|arg| arg == "update") {
                drop(server);
                drop(state);
                isolated.restore_initial_lockfile()?;
                tracing::debug!("retrying with legacy backend in the same isolated workspace");
                let phase = crate::ui::PhaseStatus::new(config.verbose);
                return match crate::run_update_in_isolation(config, cli, args, &phase, isolated)? {
                    crate::IsolatedUpdateOutcome::Done => Ok(0),
                    crate::IsolatedUpdateOutcome::CargoFailed(code) => Ok(code),
                };
            }
            return Err(Unsupported(
                "filtered resolution failed after age filtering; fallback requires legacy"
                    .to_string(),
            )
            .into());
        }
        crate::write_captured_output(&output);
        return Ok(output.status.code().unwrap_or(1));
    }
    crate::write_captured_output(&output);
    let resolve_ms = resolution.elapsed().as_millis();
    let validation = Instant::now();
    state.validate(isolated.lockfile_path(), backend)?;
    let validation_ms = validation.elapsed().as_millis();
    let publication = Instant::now();
    if !dry_run {
        isolated.publish_lockfile()?;
    }
    let publication_ms = publication.elapsed().as_millis();
    drop(server);
    if config.verbose {
        eprintln!(
            "cooldown: backend={backend:?} isolation_ms={isolation_ms} prepare_ms={prepare_ms} resolve_ms={resolve_ms} validation_ms={validation_ms} publication_ms={publication_ms} total_ms={} cargo_resolutions=1 requests={} bytes={}",
            start.elapsed().as_millis(),
            state.requests.load(Ordering::Relaxed),
            state.bytes.load(Ordering::Relaxed)
        );
    }
    if !is_resolution {
        let status = crate::backend::own_cargo_with_args(args).status()?;
        return Ok(status.code().unwrap_or(1));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_line_survives_idle_and_partial_read_timeouts() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            let stop = AtomicBool::new(false);
            read_http_line(
                &mut reader,
                &mut line,
                &stop,
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();
            line
        });
        thread::sleep(Duration::from_millis(150));
        client.write_all(b"GET /config").unwrap();
        thread::sleep(Duration::from_millis(150));
        client.write_all(b".json HTTP/1.1\r\n").unwrap();
        assert_eq!(reader.join().unwrap(), "GET /config.json HTTP/1.1\r\n");
    }

    #[test]
    fn http_line_wait_stops_on_cancellation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&stop);
        let reader = thread::spawn(move || {
            read_http_line(
                &mut BufReader::new(stream),
                &mut String::new(),
                &reader_stop,
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap()
        });
        thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
        assert_eq!(reader.join().unwrap(), 0);
    }
}
