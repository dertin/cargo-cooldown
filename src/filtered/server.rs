//! Process-owned HTTP server for the filtered sparse index.
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};
use sha2::Digest;

use super::{State, Unsupported, fingerprint};
use crate::project::ProjectContext;

pub(super) struct Server {
    pub(super) address: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}
impl Server {
    pub(super) fn start(state: Arc<State>, project: &ProjectContext) -> Result<Self> {
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
        // Share the nonblocking listener itself; no duplicated OS sockets are
        // needed for threads owned by this process.
        let listener = Arc::new(listener);
        let stop = Arc::clone(&state.stopping);
        let mut server = Self {
            address,
            stop,
            workers: Vec::new(),
        };
        for _ in 0..8 {
            let listener = Arc::clone(&listener);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_does_not_retry_an_inflight_request() {
        use std::sync::{Mutex, mpsc};

        use crate::allow_rules::AllowRules;
        use crate::backend::Backend;
        use crate::config::{
            Config, FallbackAccept, IncompatiblePublishAgePolicy, LockfileBaselineMode,
        };
        use crate::filtered::{Lockfile, Registry};
        use crate::project::ProjectKind;
        use crate::registry::resolve_registry_context;

        let temp = tempfile::tempdir().unwrap();
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        upstream.set_nonblocking(true).unwrap();
        let url = format!("sparse+http://{}/", upstream.local_addr().unwrap());
        let (received_tx, received_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let finished = Arc::new(AtomicBool::new(false));
        let upstream_finished = Arc::clone(&finished);
        let upstream_worker = thread::spawn(move || {
            let mut requests = 0;
            while !upstream_finished.load(Ordering::Relaxed) {
                match upstream.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        {
                            let mut reader = BufReader::new(&mut stream);
                            loop {
                                let mut line = String::new();
                                assert!(reader.read_line(&mut line).unwrap() > 0);
                                if line == "\r\n" {
                                    break;
                                }
                            }
                        }
                        if requests == 0 || requests == 3 {
                            received_tx.send(()).unwrap();
                            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        }
                        requests += 1;
                        let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(err) => panic!("upstream accept failed: {err}"),
                }
            }
        });
        let project = ProjectContext {
            cwd: temp.path().to_path_buf(),
            kind: ProjectKind::Crate,
            workspace_root: temp.path().to_path_buf(),
            members: Vec::new(),
            active_member: None,
        };
        let config = Config {
            backend: Backend::Filtered,
            min_publish_age_seconds: 60,
            registry_min_publish_age: Default::default(),
            incompatible_publish_age: IncompatiblePublishAgePolicy::Deny,
            fallback_accept: FallbackAccept::Prompt,
            lockfile_baseline: LockfileBaselineMode::Ignore,
            now_override: None,
            ttl_seconds: 60,
            cache_dir: None,
            http_retries: 2,
            verbose: false,
            skip_registries: Vec::new(),
            allow_rules: AllowRules::default(),
        };
        let state = Arc::new(State {
            config,
            registries: vec![Registry {
                source: url.clone(),
                context: resolve_registry_context(&url, &[]).unwrap(),
                upstream: url.strip_prefix("sparse+").unwrap().to_string(),
                baseline_versions: Default::default(),
            }],
            baseline: Lockfile::default(),
            now: chrono::Utc::now(),
            per_crate: Default::default(),
            client: Mutex::new(Some(
                reqwest::blocking::Client::builder()
                    .no_proxy()
                    .build()
                    .unwrap(),
            )),
            http_proxy: None,
            cache: temp.path().join("cache"),
            crates: Mutex::new(Default::default()),
            originals: Mutex::new(Default::default()),
            requests: Default::default(),
            bytes: Default::default(),
            unsupported: Mutex::new(None),
            request_failure: Mutex::new(None),
            excluded_versions: AtomicBool::new(false),
            stopping: Arc::new(AtomicBool::new(false)),
        });
        // Normal requests still get the configured retries while the server is running.
        let retry_state = Arc::clone(&state);
        let retry = thread::spawn(move || retry_state.fetch_original(0, "config.json"));
        received_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        release_tx.send(()).unwrap();
        assert!(retry.join().unwrap().is_err());
        assert_eq!(state.requests.load(Ordering::Relaxed), 3);

        let server = Server::start(Arc::clone(&state), &project).unwrap();
        let mut client = TcpStream::connect(server.address).unwrap();
        client
            .write_all(b"GET /0/config.json HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        received_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let stopping = Arc::clone(&state.stopping);
        let (joined_tx, joined_rx) = mpsc::channel();
        let shutdown = thread::spawn(move || {
            drop(server);
            joined_tx.send(()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !stopping.load(Ordering::Relaxed) {
            assert!(Instant::now() < deadline, "shutdown did not start");
            thread::yield_now();
        }
        release_tx.send(()).unwrap();
        joined_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        shutdown.join().unwrap();
        finished.store(true, Ordering::Relaxed);
        upstream_worker.join().unwrap();
        assert_eq!(state.requests.load(Ordering::Relaxed), 4);
    }

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
