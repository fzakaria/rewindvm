"""Python SDK for Rewind VM multi-node deterministic cluster simulation.

Speaks NDJSON-RPC 2.0 to ``rewind cluster serve`` over stdin/stdout (default)
or a Unix domain socket. Zero third-party dependencies — stdlib only.

Example::

    from rewind_cluster import Cluster

    with Cluster("raft-test", latency_ms=2, jitter_us=200) as c:
        n1 = c.add_node("node1", ["/bin/sh", "-c", "..."], ip="10.0.0.1")
        n2 = c.add_node("node2", ["/bin/sh", "-c", "..."], ip="10.0.0.2")
        cli = c.add_node("client", ["/bin/sh", "-c", "..."], role="driver")
        c.start()

        n1.wait_for("ready")
        snap = c.snapshot("after-boot")
        c.partition(["node1"], ["node2", "client"])
        c.advance(ms=500)
        assert n1.cat("/tmp/state").strip() == "ok"
        c.heal()
        cli.wait_exit()
"""

from __future__ import annotations

import base64
import json
import os
import socket
import subprocess
from dataclasses import dataclass
from typing import Any, Dict, Iterable, List, Optional, Sequence, Tuple, Union

__all__ = [
    "Cluster",
    "ClusterError",
    "NodeHandle",
    "Nemesis",
    "WaitResult",
]


class ClusterError(RuntimeError):
    """Raised when the Rewind cluster RPC server returns an error."""


@dataclass
class Nemesis:
    """Schedule-driven fault injection profile (active when schedule > 0 or nemesis=True)."""

    tick_ms: int = 100
    partition_prob_ppm: int = 150_000
    heal_prob_ppm: int = 250_000
    crash_prob_ppm: int = 100_000
    restart_prob_ppm: int = 300_000
    max_crashed: int = 1

    def to_dict(self) -> Dict[str, Any]:
        return {
            "tick_ms": self.tick_ms,
            "partition_prob_ppm": self.partition_prob_ppm,
            "heal_prob_ppm": self.heal_prob_ppm,
            "crash_prob_ppm": self.crash_prob_ppm,
            "restart_prob_ppm": self.restart_prob_ppm,
            "max_crashed": self.max_crashed,
        }


@dataclass
class WaitResult:
    matched: bool
    line: Optional[str]
    cluster_ns: int
    cluster_step: int
    node_step: int

    @property
    def cluster_ms(self) -> float:
        return self.cluster_ns / 1_000_000.0


class NodeHandle:
    """Convenience handle for one node in a :class:`Cluster`."""

    def __init__(self, cluster: "Cluster", name: str) -> None:
        self._cluster = cluster
        self.name = name

    def wait_for(
        self,
        contains: str,
        *,
        stream: str = "output",
        timeout_ms: Optional[int] = 10_000,
        from_start: bool = False,
        wall_timeout_ms: Optional[int] = None,
        check: bool = True,
    ) -> WaitResult:
        return self._cluster.wait_for(
            self.name,
            contains,
            stream=stream,
            timeout_ms=timeout_ms,
            from_start=from_start,
            wall_timeout_ms=wall_timeout_ms,
            check=check,
        )

    def wait_exit(
        self,
        *,
        timeout_ms: Optional[int] = 30_000,
        wall_timeout_ms: Optional[int] = None,
    ) -> Dict[str, Any]:
        return self._cluster.wait_exit(
            self.name, timeout_ms=timeout_ms, wall_timeout_ms=wall_timeout_ms
        )

    def crash(self) -> Dict[str, Any]:
        return self._cluster.crash(self.name)

    def restart(self) -> Dict[str, Any]:
        return self._cluster.restart(self.name)

    def isolate(self, *, symmetric: bool = True) -> Dict[str, Any]:
        return self._cluster.partition([self.name], None, symmetric=symmetric)

    def heal(self) -> Dict[str, Any]:
        return self._cluster.heal([self.name])

    def cat(self, path: str, *, pid: Optional[int] = None) -> str:
        return self._cluster.cat(self.name, path, pid=pid)

    def cat_bytes(self, path: str, *, pid: Optional[int] = None) -> bytes:
        return self._cluster.cat_bytes(self.name, path, pid=pid)

    def files(self, path: str, *, pid: Optional[int] = None) -> List[Dict[str, Any]]:
        return self._cluster.files(self.name, path, pid=pid)

    def logs(self, *, stream: str = "output", since: int = 0) -> List[str]:
        return self._cluster.logs(self.name, stream=stream, since=since)["lines"]


class Cluster:
    """Interactive controller for a deterministic multi-node Rewind VM cluster."""

    def __init__(
        self,
        name: str = "cluster",
        *,
        subnet: str = "10.0.0.0/24",
        latency_ms: Optional[int] = 1,
        latency_us: Optional[int] = None,
        jitter_ms: Optional[int] = None,
        jitter_us: Optional[int] = None,
        drop_ppm: int = 0,
        duplicate_ppm: int = 0,
        mem_mib: int = 128,
        cores: int = 1,
        stop_when: str = "driver",
        max_virtual_ms: int = 60_000,
        slice_us: Optional[int] = None,
        packages: Optional[Sequence[str]] = None,
        seed: Optional[int] = None,
        schedule: Optional[int] = None,
        epoch: Optional[int] = None,
        nemesis: Optional[Union[Nemesis, Dict[str, Any]]] = None,
        nemesis_enabled: Optional[bool] = None,
        keyframes: Optional[bool] = None,
        echo: bool = False,
        clock: Optional[str] = None,
        cpu: Optional[str] = None,
        rewind_bin: Optional[str] = None,
        socket_path: Optional[str] = None,
    ) -> None:
        self.name = name
        self._network: Dict[str, Any] = {
            "subnet": subnet,
            "drop_ppm": drop_ppm,
            "duplicate_ppm": duplicate_ppm,
        }
        if latency_us is not None:
            self._network["latency_us"] = latency_us
        elif latency_ms is not None:
            self._network["latency_ms"] = latency_ms
        if jitter_us is not None:
            self._network["jitter_us"] = jitter_us
        elif jitter_ms is not None:
            self._network["jitter_ms"] = jitter_ms

        self._mem_mib = mem_mib
        self._cores = cores
        self._stop_when = stop_when
        self._max_virtual_ms = max_virtual_ms
        self._slice_us = slice_us
        self._packages = list(packages) if packages is not None else []
        self._seed = seed
        self._schedule = schedule
        self._epoch = epoch
        self._nemesis = nemesis.to_dict() if isinstance(nemesis, Nemesis) else nemesis
        self._nemesis_enabled = nemesis_enabled
        self._keyframes = keyframes
        self._echo = echo
        self._clock = clock or os.environ.get("REWIND_CLOCK")
        self._cpu = cpu
        self._rewind_bin = rewind_bin or os.environ.get("REWIND_BIN") or "rewind"
        self._socket_path = socket_path or os.environ.get("REWIND_CLUSTER_SOCK")

        self._nodes: List[Dict[str, Any]] = []
        self._links: List[Dict[str, Any]] = []
        self._proc: Optional[subprocess.Popen[bytes]] = None
        self._sock: Optional[socket.socket] = None
        self._rfile: Any = None
        self._wfile: Any = None
        self._req_id = 0
        self._started = False
        self._finished = False
        self.result: Optional[Dict[str, Any]] = None

    def add_node(
        self,
        name: str,
        command: Union[str, Sequence[str]],
        *,
        ip: Optional[str] = None,
        role: str = "service",
        root: Optional[str] = None,
        packages: Optional[Sequence[str]] = None,
        env: Optional[Union[Dict[str, str], Sequence[str]]] = None,
        cwd: Optional[str] = None,
        uid: Optional[int] = None,
        gid: Optional[int] = None,
        mem_mib: Optional[int] = None,
        cores: Optional[int] = None,
        clock_skew_ms: int = 0,
        clock_drift_ppm: int = 0,
        files: Optional[Dict[str, str]] = None,
    ) -> NodeHandle:
        """Registers a node before calling :meth:`start`."""
        if self._started:
            raise ClusterError("cannot add nodes after cluster.start()")
        cmd = ["/bin/sh", "-c", command] if isinstance(command, str) else list(command)
        env_list: List[str] = []
        if isinstance(env, dict):
            env_list = [f"{k}={v}" for k, v in env.items()]
        elif env is not None:
            env_list = list(env)
        file_list = (
            [{"path": p, "contents": c} for p, c in files.items()] if files else []
        )
        node: Dict[str, Any] = {
            "name": name,
            "role": role,
            "command": cmd,
            "env": env_list,
            "clock_skew_ms": clock_skew_ms,
            "clock_drift_ppm": clock_drift_ppm,
            "files": file_list,
        }
        if ip is not None:
            node["ip"] = ip
        if root is not None:
            node["root"] = root
        if packages is not None:
            node["packages"] = list(packages)
        if cwd is not None:
            node["cwd"] = cwd
        if uid is not None:
            node["uid"] = uid
        if gid is not None:
            node["gid"] = gid
        if mem_mib is not None:
            node["mem_mib"] = mem_mib
        if cores is not None:
            node["cores"] = cores
        self._nodes.append(node)
        return NodeHandle(self, name)

    def add_link(
        self,
        src: str,
        dst: str,
        *,
        latency_ms: Optional[int] = None,
        latency_us: Optional[int] = None,
        jitter_ms: Optional[int] = None,
        jitter_us: Optional[int] = None,
        drop_ppm: Optional[int] = None,
        bidirectional: bool = True,
    ) -> None:
        """Adds an initial link override before :meth:`start`."""
        pairs = [(src, dst), (dst, src)] if bidirectional else [(src, dst)]
        for a, b in pairs:
            entry: Dict[str, Any] = {"from": a, "to": b}
            if latency_ms is not None:
                entry["latency_ms"] = latency_ms
            if latency_us is not None:
                entry["latency_us"] = latency_us
            if jitter_ms is not None:
                entry["jitter_ms"] = jitter_ms
            if jitter_us is not None:
                entry["jitter_us"] = jitter_us
            if drop_ppm is not None:
                entry["drop_ppm"] = drop_ppm
            self._links.append(entry)

    def _connect(self) -> None:
        if self._wfile is not None:
            return
        if self._socket_path:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.connect(self._socket_path)
            self._sock = sock
            self._rfile = sock.makefile("rb")
            self._wfile = sock.makefile("wb")
            return
        cmd = [self._rewind_bin, "cluster", "serve"]
        if self._clock:
            cmd.extend(["--clock", self._clock])
        if self._cpu:
            cmd.extend(["--cpu", self._cpu])
        self._proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=None,
        )
        assert self._proc.stdin is not None and self._proc.stdout is not None
        self._wfile = self._proc.stdin
        self._rfile = self._proc.stdout

    def _call(self, method: str, params: Optional[Dict[str, Any]] = None) -> Any:
        self._connect()
        self._req_id += 1
        payload = {
            "jsonrpc": "2.0",
            "id": self._req_id,
            "method": method,
            "params": params or {},
        }
        line = (json.dumps(payload) + "\n").encode("utf-8")
        self._wfile.write(line)
        self._wfile.flush()
        raw = self._rfile.readline()
        if not raw:
            raise ClusterError(f"rewind cluster serve closed connection during {method}")
        resp = json.loads(raw.decode("utf-8"))
        if "error" in resp and resp["error"] is not None:
            raise ClusterError(resp["error"].get("message", str(resp["error"])))
        return resp.get("result")

    def start(self) -> Dict[str, Any]:
        """Boots all configured VMs and begins deterministic recording."""
        if not self._nodes:
            raise ClusterError("add at least one node before calling start()")
        params: Dict[str, Any] = {
            "name": self.name,
            "mem_mib": self._mem_mib,
            "cores": self._cores,
            "stop_when": self._stop_when,
            "max_virtual_ms": self._max_virtual_ms,
            "echo": self._echo,
            "network": self._network,
            "links": self._links,
            "packages": self._packages,
            "nodes": self._nodes,
        }
        if self._slice_us is not None:
            params["slice_us"] = self._slice_us
        if self._seed is not None:
            params["seed"] = self._seed
        if self._schedule is not None:
            params["schedule"] = self._schedule
        if self._epoch is not None:
            params["epoch"] = self._epoch
        if self._nemesis is not None:
            params["nemesis"] = self._nemesis
        if self._nemesis_enabled is not None:
            params["nemesis_enabled"] = self._nemesis_enabled
        if self._keyframes is not None:
            params["keyframes"] = self._keyframes

        res = self._call("cluster.start", params)
        self._started = True
        return res

    def status(self) -> Dict[str, Any]:
        return self._call("cluster.status")

    def advance(
        self,
        *,
        ms: Optional[int] = None,
        us: Optional[int] = None,
        ns: Optional[int] = None,
        until_ms: Optional[int] = None,
        wall_timeout_ms: Optional[int] = None,
    ) -> Dict[str, Any]:
        params: Dict[str, Any] = {}
        if ms is not None:
            params["ms"] = ms
        if us is not None:
            params["us"] = us
        if ns is not None:
            params["ns"] = ns
        if until_ms is not None:
            params["until_ms"] = until_ms
        if wall_timeout_ms is not None:
            params["wall_timeout_ms"] = wall_timeout_ms
        return self._call("cluster.advance", params)

    def wait_for(
        self,
        node: str,
        contains: str,
        *,
        stream: str = "output",
        timeout_ms: Optional[int] = 10_000,
        from_start: bool = False,
        wall_timeout_ms: Optional[int] = None,
        check: bool = True,
    ) -> WaitResult:
        params: Dict[str, Any] = {
            "node": node,
            "contains": contains,
            "stream": stream,
            "from_start": from_start,
        }
        if timeout_ms is not None:
            params["timeout_ms"] = timeout_ms
        if wall_timeout_ms is not None:
            params["wall_timeout_ms"] = wall_timeout_ms
        res = self._call("cluster.wait_for", params)
        out = WaitResult(
            matched=bool(res["matched"]),
            line=res.get("line"),
            cluster_ns=int(res["cluster_ns"]),
            cluster_step=int(res["cluster_step"]),
            node_step=int(res["node_step"]),
        )
        if check and not out.matched:
            raise AssertionError(
                f"timed out waiting for {node!r} ({stream}) to emit {contains!r} "
                f"at {out.cluster_ms:.1f}ms"
            )
        return out

    def wait_exit(
        self,
        node: Optional[str] = None,
        *,
        timeout_ms: Optional[int] = 30_000,
        wall_timeout_ms: Optional[int] = None,
    ) -> Dict[str, Any]:
        params: Dict[str, Any] = {}
        if node is not None:
            params["node"] = node
        if timeout_ms is not None:
            params["timeout_ms"] = timeout_ms
        if wall_timeout_ms is not None:
            params["wall_timeout_ms"] = wall_timeout_ms
        return self._call("cluster.wait_exit", params)

    def partition(
        self,
        a: Iterable[str],
        b: Optional[Iterable[str]] = None,
        *,
        symmetric: bool = True,
    ) -> Dict[str, Any]:
        params: Dict[str, Any] = {"a": list(a), "symmetric": symmetric}
        if b is not None:
            params["b"] = list(b)
        return self._call("cluster.partition", params)

    def heal(
        self,
        a: Optional[Iterable[str]] = None,
        b: Optional[Iterable[str]] = None,
    ) -> Dict[str, Any]:
        params: Dict[str, Any] = {}
        if a is not None:
            params["a"] = list(a)
        if b is not None:
            params["b"] = list(b)
        return self._call("cluster.heal", params)

    def set_link(
        self,
        src: str,
        dst: str,
        *,
        latency_ms: Optional[int] = None,
        latency_us: Optional[int] = None,
        jitter_ms: Optional[int] = None,
        jitter_us: Optional[int] = None,
        drop_ppm: Optional[int] = None,
        duplicate_ppm: Optional[int] = None,
        bidirectional: bool = True,
    ) -> Dict[str, Any]:
        params: Dict[str, Any] = {
            "from": src,
            "to": dst,
            "bidirectional": bidirectional,
        }
        if latency_ms is not None:
            params["latency_ms"] = latency_ms
        if latency_us is not None:
            params["latency_us"] = latency_us
        if jitter_ms is not None:
            params["jitter_ms"] = jitter_ms
        if jitter_us is not None:
            params["jitter_us"] = jitter_us
        if drop_ppm is not None:
            params["drop_ppm"] = drop_ppm
        if duplicate_ppm is not None:
            params["duplicate_ppm"] = duplicate_ppm
        return self._call("cluster.set_link", params)

    def crash(self, node: str) -> Dict[str, Any]:
        return self._call("cluster.crash", {"node": node})

    def restart(self, node: str) -> Dict[str, Any]:
        return self._call("cluster.restart", {"node": node})

    def logs(
        self, node: str, *, stream: str = "output", since: int = 0
    ) -> Dict[str, Any]:
        return self._call(
            "cluster.logs", {"node": node, "stream": stream, "since": since}
        )

    def net_events(self, *, since: int = 0) -> Dict[str, Any]:
        return self._call("cluster.net_events", {"since": since})

    def cat_bytes(
        self, node: str, path: str, *, pid: Optional[int] = None
    ) -> bytes:
        params: Dict[str, Any] = {"node": node, "path": path}
        if pid is not None:
            params["pid"] = pid
        res = self._call("cluster.cat", params)
        if not res.get("found"):
            raise FileNotFoundError(res.get("error") or f"{node}:{path} not found")
        return base64.b64decode(res["content_base64"])

    def cat(self, node: str, path: str, *, pid: Optional[int] = None) -> str:
        return self.cat_bytes(node, path, pid=pid).decode("utf-8", errors="replace")

    def files(
        self, node: str, path: str, *, pid: Optional[int] = None
    ) -> List[Dict[str, Any]]:
        params: Dict[str, Any] = {"node": node, "path": path}
        if pid is not None:
            params["pid"] = pid
        res = self._call("cluster.files", params)
        if not res.get("found"):
            raise FileNotFoundError(res.get("error") or f"{node}:{path} not found")
        return res["entries"]

    def snapshot(self, name: Optional[str] = None) -> str:
        params = {"name": name} if name is not None else {}
        res = self._call("cluster.snapshot", params)
        return str(res["snapshot"])

    def restore(self, snapshot: str) -> Dict[str, Any]:
        return self._call("cluster.restore", {"snapshot": snapshot})

    def finish(
        self,
        *,
        verdict: Optional[str] = None,
        reason: Optional[str] = None,
    ) -> Dict[str, Any]:
        if self._finished:
            assert self.result is not None
            return self.result
        params: Dict[str, Any] = {}
        if verdict is not None:
            params["verdict"] = verdict
        if reason is not None:
            params["reason"] = reason
        res = self._call("cluster.finish", params)
        self._finished = True
        self.result = res
        return res

    def close(self) -> None:
        try:
            if self._wfile is not None:
                try:
                    self._call("shutdown")
                except Exception:
                    pass
        finally:
            if self._sock is not None:
                self._sock.close()
                self._sock = None
            if self._proc is not None:
                if self._proc.stdin:
                    self._proc.stdin.close()
                self._proc.wait(timeout=10)
                self._proc = None
            self._rfile = None
            self._wfile = None

    def __enter__(self) -> "Cluster":
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        try:
            if self._started and not self._finished:
                if exc is not None:
                    self.finish(
                        verdict="failed",
                        reason=f"{exc_type.__name__}: {exc}",
                    )
                else:
                    self.finish()
        finally:
            self.close()
