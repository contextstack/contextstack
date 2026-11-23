"""Host-owned MCP stdio bridge: sandbox children receive a socket capability, never a credential."""
import copy
import hmac
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import threading

MAX_MCP_CONNECTIONS = 32
MCP_AUTH_TIMEOUT_S = 5.0


class HostMCPBridge:
    """Serve a trusted host-built contextstack MCP command outside the CLI sandbox."""

    def __init__(self, config, workspace, env=None, secret_dir=None, *, loopback=False):
        servers = config.get("mcpServers", {})
        if set(servers) != {"contextstack"}:
            raise ValueError("host MCP bridge requires only the contextstack server")
        server = servers["contextstack"]
        if set(server) - {"command", "args", "type"} or server.get("type", "stdio") != "stdio":
            raise ValueError("host MCP bridge requires a plain stdio command")
        command, args = server.get("command"), server.get("args", [])
        if not isinstance(command, str) or not isinstance(args, list) or not all(isinstance(a, str) for a in args):
            raise ValueError("invalid host MCP command")
        self.command = [command, *args]
        self.workspace, self.env = str(workspace), dict(os.environ if env is None else env)
        if loopback and secret_dir is None:
            raise ValueError("loopback MCP requires a job-private capability directory")
        self.root = None if loopback else Path(tempfile.mkdtemp(prefix="lead-mcp-", dir="/tmp"))
        self.path = None if loopback else self.root / "mcp.sock"
        self.token = secrets.token_hex(32)
        secret_root = Path(secret_dir) if secret_dir is not None else self.root
        secret_root.mkdir(mode=0o700, parents=True, exist_ok=True)
        prefix = "lane-mcp-capability-" if loopback else "mcp-capability-"
        fd, secret_path = tempfile.mkstemp(prefix=prefix, dir=secret_root)
        self.secret_path = Path(secret_path)
        with os.fdopen(fd, "w") as stream:
            stream.write(self.token)
        self.listener = socket.socket(socket.AF_INET if loopback else socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(("127.0.0.1", 0) if loopback else str(self.path))
        endpoint = f"tcp:{self.listener.getsockname()[1]}" if loopback else str(self.path)
        self.listener.listen(min(MAX_MCP_CONNECTIONS, socket.SOMAXCONN))
        self.listener.settimeout(0.2)
        self.stopped = threading.Event()
        self.lock = threading.Lock()
        self.children, self.connections, self.handlers = set(), set(), set()
        self.config = copy.deepcopy(config)
        self.config["mcpServers"]["contextstack"] = {
            "command": sys.executable,
            "args": [str(Path(__file__).resolve()), "proxy", endpoint, str(self.secret_path)],
        }
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()

    def _authenticate(self, conn) -> bool:
        try:
            conn.settimeout(MCP_AUTH_TIMEOUT_S)
            auth = bytearray()
            while len(auth) <= 64:
                byte = conn.recv(1)
                if byte == b"\n" or not byte:
                    break
                auth.extend(byte)
            return bool(auth) and hmac.compare_digest(bytes(auth), self.token.encode()) and not self.stopped.is_set()
        except (OSError, ValueError):
            return False

    def _serve(self):
        while not self.stopped.is_set():
            try:
                conn, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with self.lock:
                if self.stopped.is_set() or len(self.handlers) >= MAX_MCP_CONNECTIONS:
                    conn.close()
                    continue
            if not self._authenticate(conn):
                conn.close()
                continue
            conn.settimeout(None)
            with self.lock:
                if self.stopped.is_set():
                    conn.close()
                    continue
                self.connections.add(conn)
                handler = threading.Thread(target=self._handle, args=(conn,), daemon=True)
                self.handlers.add(handler)
            handler.start()

    def _handle(self, conn):
        proc = sender = None
        try:
            with self.lock:
                if self.stopped.is_set():
                    return
                proc = subprocess.Popen(self.command, cwd=self.workspace, env=self.env,
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL)
                self.children.add(proc)

            def feed():
                try:
                    while data := conn.recv(65536):
                        proc.stdin.write(data)
                        proc.stdin.flush()
                except (OSError, ValueError):
                    pass
                finally:
                    try:
                        proc.stdin.close()
                    except OSError:
                        pass

            sender = threading.Thread(target=feed, daemon=True)
            sender.start()
            while data := os.read(proc.stdout.fileno(), 65536):
                conn.sendall(data)
        except (OSError, ValueError):
            pass
        finally:
            try:
                conn.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            if proc:
                if proc.poll() is None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait()
                if sender:
                    sender.join(timeout=2)
                proc.stdout.close()
                with self.lock:
                    self.children.discard(proc)
            with self.lock:
                self.connections.discard(conn)
                self.handlers.discard(threading.current_thread())
            conn.close()

    def close(self):
        self.stopped.set()
        self.listener.close()
        with self.lock:
            connections, children = list(self.connections), list(self.children)
        for conn in connections:
            try:
                conn.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        for proc in children:
            if proc.poll() is None:
                proc.terminate()
        self.thread.join(timeout=2)
        for handler in list(self.handlers):
            handler.join(timeout=3)
        self.secret_path.unlink(missing_ok=True)
        if self.root is not None:
            try:
                shutil.rmtree(self.root)
            except FileNotFoundError:
                pass


def proxy(endpoint, secret_path):
    token = Path(secret_path).read_text()
    if endpoint.startswith("tcp:"):
        address, family = ("127.0.0.1", int(endpoint[4:])), socket.AF_INET
    else:
        address, family = endpoint, socket.AF_UNIX
    with socket.socket(family, socket.SOCK_STREAM) as conn:
        conn.connect(address)
        conn.sendall(token.encode() + b"\n")

        def feed():
            try:
                while data := os.read(sys.stdin.fileno(), 65536):
                    conn.sendall(data)
                conn.shutdown(socket.SHUT_WR)
            except OSError:
                pass

        threading.Thread(target=feed, daemon=True).start()
        while data := conn.recv(65536):
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()


if __name__ == "__main__":
    if len(sys.argv) != 4 or sys.argv[1] != "proxy":
        raise SystemExit("usage: _lead_mcp.py proxy SOCKET CAPABILITY_FILE")
    proxy(sys.argv[2], sys.argv[3])
