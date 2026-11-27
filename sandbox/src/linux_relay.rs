//! Per-run TCP loopback forwards. The host side always dials one prevalidated
//! IPv4 loopback port, and the private network namespace reaches it via a
//! single bind-mounted Unix socket scoped by Landlock.
use std::collections::BTreeSet;
use std::io;
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use crate::cli_executor::ExecutorRequest;
use crate::failure::{Failure, unenforceable};
use crate::widening::DEFAULT_RESERVED_PORTS;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 32;

pub fn checked_ports(request: &ExecutorRequest) -> Result<BTreeSet<u16>, Failure> {
    let mut ports: BTreeSet<_> = request.loopback_ports.iter().copied().collect();
    if let Some(port) = request
        .adapter
        .as_ref()
        .and_then(|a| a.localhost_port.as_ref())
    {
        let port = port.to_string();
        if port == "*" {
            if ports.is_empty() {
                return Err(unenforceable(
                    "adapter requires explicit Linux loopback endpoints",
                ));
            }
        } else {
            let port = port
                .parse::<u16>()
                .map_err(|_| unenforceable("invalid adapter port"))?;
            if ports.iter().any(|candidate| *candidate != port) {
                return Err(unenforceable(
                    "loopback endpoint is outside the adapter policy",
                ));
            }
            ports.insert(port);
        }
    }
    if request.reserved_ports.contains(&0)
        || ports.iter().any(|port| {
            *port == 0
                || DEFAULT_RESERVED_PORTS.contains(port)
                || request.reserved_ports.contains(port)
        })
    {
        return Err(unenforceable("invalid or reserved executor loopback port"));
    }
    Ok(ports)
}

#[derive(Default)]
pub struct HostRelay {
    pub endpoints: Vec<(u16, PathBuf)>,
    stop: Arc<AtomicBool>,
    listeners: Vec<thread::JoinHandle<()>>,
    directory: Option<tempfile::TempDir>,
}

impl HostRelay {
    pub fn start(ports: &BTreeSet<u16>) -> io::Result<Self> {
        let mut relay = Self::default();
        if ports.is_empty() {
            return Ok(relay);
        }
        // Linux sockaddr_un paths are limited to 107 bytes.
        relay.directory = Some(
            tempfile::Builder::new()
                .prefix("cs-relay-")
                .tempdir_in("/tmp")?,
        );
        let Some(directory) = &relay.directory else {
            unreachable!()
        };
        let active = Arc::new(AtomicUsize::new(0));
        for &port in ports {
            let path = directory.path().join(port.to_string());
            let listener = UnixListener::bind(&path)?;
            listener.set_nonblocking(true)?;
            let stop = Arc::clone(&relay.stop);
            let active = Arc::clone(&active);
            relay.listeners.push(thread::Builder::new().spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((unix, _)) => worker(&active, move || {
                            if let Ok(tcp) = TcpStream::connect_timeout(
                                &(Ipv4Addr::LOCALHOST, port).into(),
                                CONNECT_TIMEOUT,
                            ) {
                                copy_streams(&tcp, &unix);
                            }
                        }),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            })?);
            relay.endpoints.push((port, path));
        }
        Ok(relay)
    }
}

impl Drop for HostRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for listener in self.listeners.drain(..) {
            let _ = listener.join();
        }
    }
}

pub fn start_sandbox(port: u16, socket: &Path) -> io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    Command::new("/.cs-executor-launch")
        .arg("--executor-loopback-relay")
        .arg(socket)
        .stdin(Stdio::from(OwnedFd::from(listener)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

pub fn serve_sandbox(socket: &Path) -> Result<i32, Failure> {
    let map = |e: io::Error| unenforceable(format!("executor loopback relay: {e}"));
    let listener = TcpListener::from(io::stdin().as_fd().try_clone_to_owned().map_err(map)?);
    let active = Arc::new(AtomicUsize::new(0));
    for tcp in listener.incoming() {
        let tcp = tcp.map_err(map)?;
        let socket = socket.to_path_buf();
        worker(&active, move || {
            if let Ok(unix) = UnixStream::connect(socket) {
                copy_streams(&tcp, &unix);
            }
        });
    }
    Ok(0)
}

fn worker(active: &Arc<AtomicUsize>, run: impl FnOnce() + Send + 'static) {
    struct Slot(Arc<AtomicUsize>);
    impl Drop for Slot {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Relaxed);
        }
    }
    if active.fetch_add(1, Ordering::Relaxed) >= MAX_CONNECTIONS {
        active.fetch_sub(1, Ordering::Relaxed);
        return;
    }
    let slot = Slot(Arc::clone(active));
    let _ = thread::Builder::new().spawn(move || {
        let _slot = slot;
        run();
    });
}

fn copy_streams(tcp: &TcpStream, unix: &UnixStream) {
    thread::scope(|scope| {
        scope.spawn(|| {
            let (mut input, mut output) = (tcp, unix);
            let _ = io::copy(&mut input, &mut output);
            let _ = unix.shutdown(Shutdown::Write);
        });
        let (mut input, mut output) = (unix, tcp);
        let _ = io::copy(&mut input, &mut output);
        let _ = tcp.shutdown(Shutdown::Write);
    });
}
