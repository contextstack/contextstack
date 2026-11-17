//! TCP and unix accept loops with a cap on in-flight HTTP connections.

use crate::gateway::Gateway;
use crate::server;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// Limits and timeouts for connection admission.
pub struct Gate {
    pub inflight: Arc<Semaphore>,
    pub accept_timeout: Duration,
    pub header_read_timeout: Duration,
}

impl Gate {
    pub fn new(settings: &crate::settings::Settings) -> Self {
        Self {
            inflight: Arc::new(Semaphore::new(settings.max_inflight)),
            accept_timeout: settings.inflight_accept_timeout,
            header_read_timeout: settings.header_read_timeout,
        }
    }
}

pub async fn accept_tcp(
    listener: TcpListener,
    gw: Arc<Gateway>,
    graceful: Arc<GracefulShutdown>,
    stop: CancellationToken,
    gate: Arc<Gate>,
) {
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            () = stop.cancelled() => return,
        };
        if let Ok((stream, addr)) = accepted {
            let _ = stream.set_nodelay(true);
            let gw = Arc::clone(&gw);
            let graceful = Arc::clone(&graceful);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                admit_tcp(stream, gw, addr.ip(), graceful, gate).await;
            });
        }
    }
}

pub async fn accept_unix(
    listener: UnixListener,
    gw: Arc<Gateway>,
    graceful: Arc<GracefulShutdown>,
    stop: CancellationToken,
    gate: Arc<Gate>,
) {
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            () = stop.cancelled() => return,
        };
        if let Ok((stream, _)) = accepted {
            let gw = Arc::clone(&gw);
            let graceful = Arc::clone(&graceful);
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                admit_unix(stream, gw, graceful, gate).await;
            });
        }
    }
}

async fn admit_tcp(
    stream: TcpStream,
    gw: Arc<Gateway>,
    peer_ip: IpAddr,
    graceful: Arc<GracefulShutdown>,
    gate: Arc<Gate>,
) {
    match tokio::time::timeout(gate.accept_timeout, gate.inflight.clone().acquire_owned()).await {
        Ok(Ok(permit)) => serve(
            TokioIo::new(stream),
            gw,
            server::Peer::Tcp(peer_ip),
            permit,
            gate.header_read_timeout,
            &graceful,
        ),
        _ => reject_busy(stream, gate.accept_timeout).await,
    }
}

async fn admit_unix(
    stream: UnixStream,
    gw: Arc<Gateway>,
    graceful: Arc<GracefulShutdown>,
    gate: Arc<Gate>,
) {
    match tokio::time::timeout(gate.accept_timeout, gate.inflight.clone().acquire_owned()).await {
        Ok(Ok(permit)) => serve(
            TokioIo::new(stream),
            gw,
            server::Peer::Unix,
            permit,
            gate.header_read_timeout,
            &graceful,
        ),
        _ => reject_busy(stream, gate.accept_timeout).await,
    }
}

async fn reject_busy<S>(mut stream: S, accept_timeout: Duration)
where
    S: AsyncWriteExt + Unpin,
{
    let retry_ms = i64::try_from(accept_timeout.as_millis()).unwrap_or(i64::MAX);
    let e = crate::errors::WireError::busy(retry_ms);
    let body = serde_json::json!({"status": "error", "error": e});
    let mut body = serde_json::to_string(&body).unwrap_or_else(|_| "{}".into());
    body.push('\n');
    let resp = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn serve<I>(
    io: I,
    gw: Arc<Gateway>,
    peer: server::Peer,
    permit: OwnedSemaphorePermit,
    header_read_timeout: Duration,
    graceful: &GracefulShutdown,
) where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let svc = hyper::service::service_fn(move |req| server::handle(Arc::clone(&gw), peer, req));
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(header_read_timeout);
    let conn = graceful.watch(builder.serve_connection(io, svc));
    tokio::spawn(async move {
        let _permit = permit;
        let _ = conn.await;
    });
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener as StdListener;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn pick_port() -> Option<u16> {
        let l = StdListener::bind("127.0.0.1:0").ok()?;
        let port = l.local_addr().ok()?.port();
        Some(port)
    }

    struct GatewayProc {
        child: std::process::Child,
        port: u16,
        _home: tempfile::TempDir,
    }

    impl GatewayProc {
        fn spawn(env_extra: &[(&str, &str)]) -> Option<Self> {
            let home = tempfile::tempdir().ok()?;
            let port = pick_port()?;
            let models = home.path().join("models.json");
            std::fs::write(&models, r#"{"models":[]}"#).ok()?;
            let bin = std::env::var("CARGO_BIN_EXE_cs-gateway").ok()?;
            let mut cmd = Command::new(bin);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .env_clear()
                .env("HOME", home.path())
                .env("CSAGENT_BIND", "127.0.0.1")
                .env("CSGATEWAY_PORT", port.to_string())
                .env("CSAGENT_MODELS_CONFIG", &models)
                .env("CSGATEWAY_AUTH_REQUIRED", "false")
                .env("CSGATEWAY_CREDENTIALS", "off")
                .env("CSGATEWAY_DISCOVERY_HEALTH", "off")
                .env("CSGATEWAY_PROVIDERS_DIR", "off")
                .env("CSGATEWAY_DISCOVERY_STALE_AFTER", "2h");
            for (k, v) in env_extra {
                cmd.env(k, v);
            }
            let mut child = cmd.spawn().ok()?;
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline {
                if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                    let _ = write!(
                        s,
                        "GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
                    );
                    let mut buf = vec![0u8; 512];
                    if s.read(&mut buf).unwrap_or(0) > 0 {
                        return Some(Self {
                            child,
                            port,
                            _home: home,
                        });
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }

    impl Drop for GatewayProc {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn http_exchange(
        port: u16,
        write: impl FnOnce(&mut std::net::TcpStream),
    ) -> Option<(u16, String)> {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
        let _ = s.set_read_timeout(Some(Duration::from_secs(30)));
        write(&mut s);
        let mut buf = vec![0u8; 8192];
        let n = s.read(&mut buf).unwrap_or(0);
        let text = String::from_utf8_lossy(&buf[..n]);
        let status = text
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        Some((status, text.into_owned()))
    }

    #[test]
    fn second_connection_gets_busy_when_cap_is_one() {
        let Some(gw) = GatewayProc::spawn(&[
            ("CSGATEWAY_MAX_INFLIGHT", "1"),
            ("CSGATEWAY_INFLIGHT_ACCEPT_TIMEOUT", "1s"),
        ]) else {
            return;
        };
        let port = gw.port;
        let hold = std::thread::spawn(move || {
            let Some(s) =
                std::net::TcpStream::connect(("127.0.0.1", port)).ok()
            else {
                return;
            };
            std::thread::sleep(Duration::from_secs(3));
            drop(s);
        });
        std::thread::sleep(Duration::from_millis(100));
        let started = Instant::now();
        let Some((status, body)) = http_exchange(gw.port, |_| {}) else {
            return;
        };
        let elapsed = started.elapsed();
        assert_eq!(status, 503, "body: {body}");
        assert!(
            elapsed < Duration::from_secs(3),
            "expected within accept timeout, took {elapsed:?}"
        );
        assert!(body.contains("gateway.busy"), "expected gateway.busy in {body}");
        assert!(
            body.contains("retryable_gateway"),
            "expected retryable_gateway in {body}"
        );
        let _ = hold.join();
    }

    #[test]
    fn silent_client_dropped_by_header_timeout() {
        let Some(gw) = GatewayProc::spawn(&[("CSGATEWAY_HEADER_READ_TIMEOUT", "1s")]) else {
            return;
        };
        let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", gw.port)) else {
            return;
        };
        let started = Instant::now();
        let mut buf = [0u8; 1];
        let n = s.read(&mut buf).unwrap_or(0);
        let elapsed = started.elapsed();
        assert_eq!(n, 0, "connection should close without a response body");
        assert!(
            elapsed >= Duration::from_millis(900) && elapsed < Duration::from_secs(5),
            "expected ~1s header timeout, took {elapsed:?}"
        );
    }

    #[test]
    fn health_works_under_default_limits() {
        let Some(gw) = GatewayProc::spawn(&[]) else {
            return;
        };
        let Some((status, body)) = http_exchange(gw.port, |s| {
            let _ = write!(
                s,
                "GET /v1/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
            );
        }) else {
            return;
        };
        assert_eq!(status, 200, "body: {body}");
        assert!(body.contains("200"), "unexpected response: {body}");
    }
}
