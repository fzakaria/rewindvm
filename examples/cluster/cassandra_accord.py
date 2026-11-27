#!/usr/bin/env python3
"""Deterministic 3-node Apache Cassandra 6.0 (CEP-15 Accord) protocol stress test.

Boots three real Apache Cassandra 6.0-alpha2 VMs with Accord enabled
(`accord.enabled: true`, `storage_compatibility_mode: NONE`, `RF=3`) and slight
inter-node clock skew (`0ms`, `+15ms`, `-10ms`) + clock drift (`0`, `+200ppm`,
`-200ppm`), plus a zero-dependency multi-threaded Python CQLv4 driver that
exercises strict-serializable, conflicting, 3-partition Accord transactions
across two tables (`bank.accounts` + `bank.txlog`):

  Setup (paid once, then snapshotted):
      Creates `bank.accounts` and `bank.txlog` with `transactional_mode = 'full'`,
      seeds alice/bob/carol = 1000 each, and captures a whole-cluster snapshot
      (`cluster-ready`) of all 3 JVMs, the driver VM, and the virtual L2 switch.

  Concurrent Conflicting Accord Storm (3 worker threads + linearizable auditor):
      Three concurrent workers pinned to `node1`, `node2`, and `node3`
      simultaneously submit overlapping 3-partition conditional transfers
      (`IF seen IS NULL AND s.balance >= amt THEN INSERT txlog ... UPDATE +/-`)
      while a 4th thread runs continuous multi-partition Accord reads asserting
      strict conservation (`total == 3000`, zero overdrafts) on every read.

  Branch A (from `cluster-ready`) — Mid-flight minority partition + lossy link + heal:
      Isolates `node3` and injects 5% packet loss, 2% duplication, and 3 ms
      jitter on `node1 <-> node2` *while transactions are actively in flight*,
      verifies the 2-node slow-path quorum keeps committing, heals mid-storm,
      and verifies linearizable catch-up directly on `node3` (`nodes=3`).

  Branch B (`c.restore("cluster-ready")`, zero JVM reboot) — Mid-flight
      asymmetric partition -> hard coordinator crash:
      Rewinds all 3 JVMs and the virtual switch back to `cluster-ready`,
      injects an asymmetric partition (`node1 -> node2` blocked while
      `node2 -> node1` stays open) mid-storm, then hard-crashes `node3`
      mid-flight and verifies the surviving 2-node Accord quorum completes all
      transactions with exact conservation (`total == 3000`, `nodes=2`).

Requires `nix` on PATH (for `.#cassandra6` and `nixpkgs#python3`), or prebuilt
EROFS images via `REWIND_CASSANDRA_EROFS` and `REWIND_PYTHON_EROFS`. Run with:
    python3 examples/cluster/cassandra_accord.py

Or explore perturbed schedules/nemesis faults automatically:
    rewind cluster check --schedules 4 --nemesis -- python3 examples/cluster/cassandra_accord.py
"""

import os
import sys
import textwrap
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "sdk" / "python"))

from rewind_cluster import Cluster

REPO_ROOT = Path(__file__).resolve().parents[2]

NODES = [
    ("node1", "10.0.0.1", "-9223372036854775808", 0, 0),
    ("node2", "10.0.0.2", "-3074457345618258603", 15, 200),
    ("node3", "10.0.0.3", "3074457345618258602", -10, -200),
]

LOGBACK_XML = """\
<configuration scan="false">
  <appender name="STDOUT" class="ch.qos.logback.core.ConsoleAppender">
    <encoder>
      <pattern>%-5level [%thread] %logger{24} - %msg%n</pattern>
    </encoder>
  </appender>
  <root level="INFO">
    <appender-ref ref="STDOUT" />
  </root>
  <logger name="org.apache.cassandra.db.compaction" level="WARN"/>
  <logger name="org.apache.cassandra.gms" level="INFO"/>
  <logger name="org.apache.cassandra.service.accord" level="INFO"/>
</configuration>
"""


def cassandra_yaml(ip: str, token: str, seeds: str) -> str:
    return textwrap.dedent(
        f"""\
        cluster_name: 'RewindAccordCluster'
        num_tokens: 1
        initial_token: {token}
        partitioner: org.apache.cassandra.dht.Murmur3Partitioner

        data_file_directories:
          - /build/data/data
        commitlog_directory: /build/data/commitlog
        saved_caches_directory: /build/data/saved_caches
        hints_directory: /build/data/hints

        storage_compatibility_mode: NONE
        accord:
          enabled: true
          journal_directory: /build/data/accord_journal
          queue_shard_count: 1
          command_store_shard_count: 1
          fast_path_update_delay: 1s

        seed_provider:
          - class_name: org.apache.cassandra.locator.SimpleSeedProvider
            parameters:
              - seeds: "{seeds}"

        listen_address: {ip}
        rpc_address: {ip}
        broadcast_rpc_address: {ip}
        storage_port: 7000
        ssl_storage_port: 7001
        start_native_transport: true
        native_transport_port: 9042
        native_transport_allow_older_protocols: true

        endpoint_snitch: SimpleSnitch
        authenticator:
          class_name: AllowAllAuthenticator
        authorizer:
          class_name: AllowAllAuthorizer
        role_manager:
          class_name: CassandraRoleManager
        network_authorizer:
          class_name: AllowAllNetworkAuthorizer
        cidr_authorizer:
          class_name: AllowAllCIDRAuthorizer

        disk_access_mode: standard
        commitlog_sync: periodic
        commitlog_sync_period: 10000ms
        commitlog_segment_size: 4MiB
        commitlog_disk_access_mode: legacy
        memtable_allocation_type: heap_buffers
        concurrent_reads: 4
        concurrent_writes: 4
        concurrent_counter_writes: 2
        concurrent_materialized_view_writes: 2
        concurrent_compactors: 1
        auto_snapshot: false
        hinted_handoff_enabled: true
        read_request_timeout: 5000ms
        write_request_timeout: 5000ms
        cas_contention_timeout: 5000ms
        request_timeout: 10000ms
        """
    )


# Embedded stdlib-only multi-threaded CQLv4 client + concurrent Accord invariant checker.
DRIVER_SCRIPT = r'''
import socket
import struct
import sys
import threading
import time

NODES = ["10.0.0.1", "10.0.0.2", "10.0.0.3"]
ACCOUNTS = ["alice", "bob", "carol"]
TXS_PER_WORKER = 10  # 3 workers * 10 = 30 conflicting 3-partition Accord txns

# CQL Native Protocol v4 opcodes
OP_ERROR = 0x00
OP_STARTUP = 0x01
OP_READY = 0x02
OP_QUERY = 0x07
OP_RESULT = 0x08

CL_ONE = 0x0001
CL_QUORUM = 0x0004


class CqlError(RuntimeError):
    def __init__(self, code: int, msg: str):
        super().__init__(f"CQL error 0x{code:04x}: {msg}")
        self.code = code
        self.msg = msg


class CqlConn:
    def __init__(self, host: str, port: int = 9042, timeout: float = 10.0):
        self.host = host
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.sock.settimeout(timeout)
        self._stream = 1
        self._startup()

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass

    def _recv_exact(self, n: int) -> bytes:
        buf = bytearray()
        while len(buf) < n:
            chunk = self.sock.recv(n - len(buf))
            if not chunk:
                raise ConnectionError(f"EOF from {self.host}")
            buf.extend(chunk)
        return bytes(buf)

    def _roundtrip(self, opcode: int, body: bytes) -> tuple[int, bytes]:
        stream = self._stream
        self._stream = (self._stream % 30000) + 1
        hdr = struct.pack("!BBhBi", 0x04, 0x00, stream, opcode, len(body))
        self.sock.sendall(hdr + body)
        rhdr = self._recv_exact(9)
        ver, _flags, _rstream, rop, rlen = struct.unpack("!BBhBi", rhdr)
        assert ver == 0x84, f"unexpected protocol response version {ver:#x}"
        rbody = self._recv_exact(rlen)
        if rop == OP_ERROR:
            code = struct.unpack_from("!i", rbody, 0)[0]
            mlen = struct.unpack_from("!H", rbody, 4)[0]
            msg = rbody[6 : 6 + mlen].decode("utf-8", "replace")
            raise CqlError(code, msg)
        return rop, rbody

    def _startup(self) -> None:
        body = (
            struct.pack("!H", 1)
            + struct.pack("!H", 11)
            + b"CQL_VERSION"
            + struct.pack("!H", 5)
            + b"3.0.0"
        )
        rop, _ = self._roundtrip(OP_STARTUP, body)
        if rop != OP_READY:
            raise RuntimeError(f"expected READY, got opcode {rop:#x}")

    def query(self, cql: str, consistency: int = CL_QUORUM) -> list[list[bytes | None]]:
        raw = cql.encode("utf-8")
        body = struct.pack("!i", len(raw)) + raw + struct.pack("!HB", consistency, 0)
        rop, rbody = self._roundtrip(OP_QUERY, body)
        if rop != OP_RESULT:
            raise RuntimeError(f"expected RESULT, got opcode {rop:#x}")
        kind = struct.unpack_from("!i", rbody, 0)[0]
        if kind != 2:  # Not ROWS
            return []
        return self._parse_rows(rbody, 4)

    def _read_short_str(self, buf: bytes, off: int) -> tuple[str, int]:
        n = struct.unpack_from("!H", buf, off)[0]
        off += 2
        return buf[off : off + n].decode("utf-8"), off + n

    def _skip_type(self, buf: bytes, off: int) -> int:
        tid = struct.unpack_from("!H", buf, off)[0]
        off += 2
        if tid == 0x0000:
            _, off = self._read_short_str(buf, off)
        elif tid in (0x0020, 0x0022):
            off = self._skip_type(buf, off)
        elif tid == 0x0021:
            off = self._skip_type(buf, off)
            off = self._skip_type(buf, off)
        elif tid == 0x0030:
            _, off = self._read_short_str(buf, off)
            _, off = self._read_short_str(buf, off)
            n = struct.unpack_from("!H", buf, off)[0]
            off += 2
            for _ in range(n):
                _, off = self._read_short_str(buf, off)
                off = self._skip_type(buf, off)
        elif tid == 0x0031:
            n = struct.unpack_from("!H", buf, off)[0]
            off += 2
            for _ in range(n):
                off = self._skip_type(buf, off)
        return off

    def _parse_rows(self, buf: bytes, off: int) -> list[list[bytes | None]]:
        flags, col_count = struct.unpack_from("!ii", buf, off)
        off += 8
        if flags & 0x0002:
            plen = struct.unpack_from("!i", buf, off)[0]
            off += 4 + max(plen, 0)
        if not (flags & 0x0004):
            global_spec = bool(flags & 0x0001)
            if global_spec:
                _, off = self._read_short_str(buf, off)
                _, off = self._read_short_str(buf, off)
            for _ in range(col_count):
                if not global_spec:
                    _, off = self._read_short_str(buf, off)
                    _, off = self._read_short_str(buf, off)
                _, off = self._read_short_str(buf, off)
                off = self._skip_type(buf, off)
        row_count = struct.unpack_from("!i", buf, off)[0]
        off += 4
        rows: list[list[bytes | None]] = []
        for _ in range(row_count):
            row: list[bytes | None] = []
            for _ in range(col_count):
                vlen = struct.unpack_from("!i", buf, off)[0]
                off += 4
                if vlen < 0:
                    row.append(None)
                else:
                    row.append(buf[off : off + vlen])
                    off += vlen
            rows.append(row)
        return rows


def decode_int(cell: bytes | None) -> int:
    assert cell is not None and len(cell) == 4, f"bad int cell: {cell!r}"
    return struct.unpack("!i", cell)[0]


def connect_with_retry(host: str, deadline: float, timeout: float = 15.0) -> CqlConn:
    last_err = None
    while time.monotonic() < deadline:
        try:
            return CqlConn(host, timeout=timeout)
        except Exception as e:
            last_err = e
            time.sleep(0.5)
    raise RuntimeError(f"failed to connect to {host}: {last_err}")


def wait_cluster_ready(conns: list[CqlConn], deadline: float) -> None:
    while time.monotonic() < deadline:
        ok = True
        for c in conns:
            try:
                rows = c.query("SELECT peer FROM system.peers_v2", CL_ONE)
                if len(rows) < 2:
                    ok = False
                    break
            except Exception:
                ok = False
                break
        if ok:
            return
        time.sleep(0.5)
    raise RuntimeError("timed out waiting for 3-node cluster membership")


def accord_read_all(conn: CqlConn) -> dict[str, int]:
    """Strictly serializable multi-partition Accord read across all 3 accounts."""
    cql = """
    BEGIN TRANSACTION
      LET a = (SELECT balance FROM bank.accounts WHERE id = 'alice');
      LET b = (SELECT balance FROM bank.accounts WHERE id = 'bob');
      LET c = (SELECT balance FROM bank.accounts WHERE id = 'carol');
      SELECT a.balance, b.balance, c.balance;
    COMMIT TRANSACTION
    """
    rows = conn.query(cql, CL_QUORUM)
    assert len(rows) == 1, f"expected 1 row from Accord read, got {rows!r}"
    return {
        "alice": decode_int(rows[0][0]),
        "bob": decode_int(rows[0][1]),
        "carol": decode_int(rows[0][2]),
    }


def accord_idempotent_transfer(
    conn: CqlConn, tx_id: str, src: str, dst: str, amount: int
) -> None:
    """Strictly serializable, idempotent 3-partition Accord transfer across 2 tables.

    Guards on `bank.txlog[tx_id]` and `s.balance >= amount` inside a single
    Accord transaction so concurrent conflicting writers and retries after
    ambiguous timeouts/partitions/crashes have exact-once semantics.
    """
    cql = f"""
    BEGIN TRANSACTION
      LET seen = (SELECT applied FROM bank.txlog WHERE tx_id = '{tx_id}');
      LET s = (SELECT balance FROM bank.accounts WHERE id = '{src}');
      IF seen IS NULL AND s.balance >= {amount} THEN
        INSERT INTO bank.txlog (tx_id, applied) VALUES ('{tx_id}', 1);
        UPDATE bank.accounts SET balance -= {amount} WHERE id = '{src}';
        UPDATE bank.accounts SET balance += {amount} WHERE id = '{dst}';
      END IF
    COMMIT TRANSACTION
    """
    conn.query(cql, CL_QUORUM)


def check_invariant(label: str, balances: dict[str, int], log: bool = True) -> None:
    total = sum(balances.values())
    if log:
        print(f"{label}: balances={balances} total={total}", flush=True)
    for acct, bal in balances.items():
        if bal < 0:
            raise AssertionError(f"OVERDRAFT on {acct}: {balances}")
    if total != 3000:
        raise AssertionError(f"CONSERVATION VIOLATION: total={total} balances={balances}")


def run_with_retry(fn, attempts: int = 10, delay: float = 0.5):
    last_err = None
    for _ in range(attempts):
        try:
            return fn()
        except Exception as e:
            last_err = e
            time.sleep(delay)
    raise RuntimeError(f"operation failed after {attempts} attempts: {last_err}")


def main() -> None:
    deadline = time.monotonic() + 240.0
    print("DRIVER_CONNECTING", flush=True)
    conns = [connect_with_retry(ip, deadline) for ip in NODES]
    wait_cluster_ready(conns, deadline)
    print("CLUSTER_3_NODES_JOINED", flush=True)

    c1, c2, c3 = conns
    c1.query(
        "CREATE KEYSPACE IF NOT EXISTS bank "
        "WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 3}",
        CL_QUORUM,
    )
    c1.query(
        "CREATE TABLE IF NOT EXISTS bank.accounts ("
        "  id text PRIMARY KEY,"
        "  balance int"
        ") WITH transactional_mode = 'full'",
        CL_QUORUM,
    )
    c1.query(
        "CREATE TABLE IF NOT EXISTS bank.txlog ("
        "  tx_id text PRIMARY KEY,"
        "  applied int"
        ") WITH transactional_mode = 'full'",
        CL_QUORUM,
    )
    run_with_retry(
        lambda: c1.query(
            "BEGIN TRANSACTION "
            "  INSERT INTO bank.accounts (id, balance) VALUES ('alice', 1000); "
            "  INSERT INTO bank.accounts (id, balance) VALUES ('bob', 1000); "
            "  INSERT INTO bank.accounts (id, balance) VALUES ('carol', 1000); "
            "COMMIT TRANSACTION",
            CL_QUORUM,
        )
    )
    check_invariant("INIT", run_with_retry(lambda: accord_read_all(c2)))
    for c in conns:
        c.close()

    # Signal the host orchestrator to snapshot the warm 3-node Accord cluster.
    print("CLUSTER_READY", flush=True)
    time.sleep(0.1)

    # Concurrent conflicting multi-partition Accord storm:
    # 3 workers targeting node1, node2, node3 simultaneously + 1 linearizable auditor.
    lock = threading.Lock()
    committed_count = 0
    in_flight_emitted = False
    mid_emitted = False
    audit_count = 0
    errors: list[BaseException] = []
    stop_audit = threading.Event()

    def worker(wid: int) -> None:
        nonlocal committed_count, in_flight_emitted, mid_emitted
        # Each worker prefers its own coordinator but fails over round-robin
        # if that coordinator is partitioned or hard-crashed mid-storm.
        hosts = [NODES[(wid + k) % 3] for k in range(3)]
        conn: CqlConn | None = None
        host_idx = 0
        try:
            for seq in range(TXS_PER_WORKER):
                tx_id = f"w{wid}-{seq}"
                src = ACCOUNTS[(wid + seq) % 3]
                dst = ACCOUNTS[(wid + seq + 1) % 3]
                amt = 15 + ((wid * 7 + seq * 11) % 45)
                ok = False
                last_err: Exception | None = None
                for attempt in range(24):
                    if conn is None:
                        target = hosts[host_idx % len(hosts)]
                        try:
                            conn = CqlConn(target, timeout=3.5)
                        except Exception as e:
                            last_err = e
                            host_idx += 1
                            time.sleep(0.15)
                            continue
                    try:
                        accord_idempotent_transfer(conn, tx_id, src, dst, amt)
                        ok = True
                        break
                    except Exception as e:
                        last_err = e
                        conn.close()
                        conn = None
                        host_idx += 1
                        time.sleep(0.2)
                if not ok:
                    raise RuntimeError(f"worker {wid} tx {tx_id} failed: {last_err}")

                with lock:
                    committed_count += 1
                    cur = committed_count
                    if cur >= 3 and not in_flight_emitted:
                        in_flight_emitted = True
                        print(f"STORM_IN_FLIGHT committed={cur}", flush=True)
                    if cur >= 15 and not mid_emitted:
                        mid_emitted = True
                        print(f"STORM_MID committed={cur}", flush=True)
        except BaseException as e:
            with lock:
                errors.append(e)
        finally:
            if conn is not None:
                conn.close()

    def auditor() -> None:
        nonlocal audit_count
        idx = 0
        while not stop_audit.is_set():
            host = NODES[idx % 3]
            idx += 1
            conn: CqlConn | None = None
            try:
                conn = CqlConn(host, timeout=3.0)
                bal = accord_read_all(conn)
                check_invariant("AUDIT", bal, log=False)
                with lock:
                    audit_count += 1
            except AssertionError as e:
                with lock:
                    errors.append(e)
                return
            except Exception:
                # Expected when probing a partitioned or crashed coordinator.
                pass
            finally:
                if conn is not None:
                    conn.close()
            time.sleep(0.15)

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(3)]
    aud = threading.Thread(target=auditor)
    aud.start()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    stop_audit.set()
    aud.join()

    if errors:
        raise errors[0]

    print(f"STORM_DONE committed={committed_count} audits={audit_count}", flush=True)

    # Give any healed node a short catch-up window, then verify linearizable
    # convergence across every reachable node.
    time.sleep(2.0)
    observed: dict[str, dict[str, int]] = {}
    ledger_count = None
    for ip in NODES:
        conn = None
        try:
            conn = CqlConn(ip, timeout=4.0)
            bal = run_with_retry(lambda c=conn: accord_read_all(c), attempts=6, delay=0.4)
            check_invariant(f"FINAL_{ip}", bal)
            observed[ip] = bal
            if ledger_count is None:
                rows = conn.query("SELECT tx_id FROM bank.txlog", CL_QUORUM)
                ledger_count = len(rows)
        except Exception as e:
            print(f"UNREACHABLE {ip}: {e}", flush=True)
        finally:
            if conn is not None:
                conn.close()

    assert len(observed) >= 2, f"expected >=2 reachable nodes, got {observed!r}"
    vals = list(observed.values())
    for ip, bal in observed.items():
        assert bal == vals[0], f"DIVERGENCE on {ip}: {bal} != {vals[0]}"
    assert ledger_count == TXS_PER_WORKER * 3, (
        f"expected {TXS_PER_WORKER * 3} applied txns in txlog, got {ledger_count}"
    )
    print(
        f"CONVERGENCE nodes={len(observed)} applied={ledger_count} "
        f"audits={audit_count} balances={vals[0]}",
        flush=True,
    )
    print("ACCORD_ALL_OK", flush=True)


if __name__ == "__main__":
    main()
'''


def main() -> None:
    cassandra_pkg = os.environ.get("REWIND_CASSANDRA_PKG", f"{REPO_ROOT}#cassandra6")
    cassandra_erofs = os.environ.get("REWIND_CASSANDRA_EROFS")
    python_erofs = os.environ.get("REWIND_PYTHON_EROFS")
    seeds = "10.0.0.1:7000"

    t0 = time.perf_counter()
    with Cluster(
        "cassandra6-accord-3node",
        latency_ms=2,
        jitter_us=300,
        mem_mib=1024,
        stop_when="driver",
        max_virtual_ms=300_000,
        slice_us=250,
    ) as c:
        node_handles = []
        for idx, (name, ip, token, skew_ms, drift_ppm) in enumerate(NODES):
            # Stagger joins cleanly (Cassandra CEP-21 TCM forbids concurrent
            # bootstraps): node2 waits for node1:9042, node3 waits for node2:9042.
            if idx == 0:
                cmd = ["cassandra-node", "/build", "/build/data"]
            else:
                prev_ip = NODES[idx - 1][1]
                cmd = [
                    "/bin/sh",
                    "-c",
                    f"until (nc -z {prev_ip} 9042 2>/dev/null || "
                    f"bash -c 'exec 3<>/dev/tcp/{prev_ip}/9042' 2>/dev/null); "
                    "do sleep 0.5; done; "
                    "exec cassandra-node /build /build/data",
                ]
            node_kwargs = {}
            if cassandra_erofs:
                node_kwargs["root"] = cassandra_erofs
            else:
                node_kwargs["packages"] = [cassandra_pkg]
            h = c.add_node(
                name,
                cmd,
                ip=ip,
                role="service",
                mem_mib=1024,
                clock_skew_ms=skew_ms,
                clock_drift_ppm=drift_ppm,
                files={
                    "/build/cassandra.yaml": cassandra_yaml(ip, token, seeds),
                    "/build/logback.xml": LOGBACK_XML,
                },
                **node_kwargs,
            )
            node_handles.append(h)

        client_kwargs = {}
        if python_erofs:
            client_kwargs["root"] = python_erofs
        else:
            client_kwargs["packages"] = ["nixpkgs#python3"]
        client = c.add_node(
            "client",
            ["python3", "-u", "/build/driver.py"],
            ip="10.0.0.4",
            role="driver",
            mem_mib=256,
            files={"/build/driver.py": DRIVER_SCRIPT},
            **client_kwargs,
        )

        c.start()
        try:
            for h in node_handles:
                h.wait_for(
                    "Starting listening for CQL clients",
                    from_start=True,
                    timeout_ms=120_000,
                )

            client.wait_for("CLUSTER_3_NODES_JOINED", from_start=True, timeout_ms=60_000)
            client.wait_for("CLUSTER_READY", from_start=True, timeout_ms=60_000)
            t_boot = time.perf_counter()

            # Snapshot the warm 3-node Accord cluster (VMs + full virtual switch)
            # so we can branch into multiple mid-flight fault timelines without rebooting!
            snap_ready = c.snapshot("cluster-ready")
            t_snap = time.perf_counter()

            # === Branch A: Mid-flight minority partition + lossy/jittery quorum link + heal ===
            client.wait_for("STORM_IN_FLIGHT", from_start=True, timeout_ms=60_000)
            c.partition(["node3"], ["node1", "node2", "client"])
            c.set_link(
                "node1",
                "node2",
                latency_ms=8,
                jitter_ms=3,
                drop_ppm=50_000,
                duplicate_ppm=20_000,
            )

            client.wait_for("STORM_MID", from_start=True, timeout_ms=90_000)
            c.heal()
            c.set_link(
                "node1",
                "node2",
                latency_ms=2,
                jitter_us=300,
                drop_ppm=0,
                duplicate_ppm=0,
            )

            conv_a = client.wait_for("CONVERGENCE", from_start=True, timeout_ms=90_000)
            assert "nodes=3 " in (conv_a.line or ""), f"Branch A expected 3 converged nodes: {conv_a.line}"
            client.wait_for("ACCORD_ALL_OK", from_start=True, timeout_ms=30_000)
            t_branch_a = time.perf_counter()
            print(f"[Branch A] {conv_a.line} ({t_branch_a - t_snap:.2f}s wall)", flush=True)

            # === Rewind to `cluster-ready` (zero JVM reboot!) ===
            c.restore(snap_ready)
            t_restore = time.perf_counter()

            # === Branch B: Mid-flight asymmetric partition -> hard crash of node3 ===
            client.wait_for("STORM_IN_FLIGHT", from_start=True, timeout_ms=60_000)
            c.partition(["node1"], ["node2"], symmetric=False)

            client.wait_for("STORM_MID", from_start=True, timeout_ms=90_000)
            c.heal()
            node_handles[2].crash()

            conv_b = client.wait_for("CONVERGENCE", from_start=True, timeout_ms=90_000)
            assert "nodes=2 " in (conv_b.line or ""), f"Branch B expected 2 surviving nodes: {conv_b.line}"
            client.wait_for("ACCORD_ALL_OK", from_start=True, timeout_ms=30_000)
            res = client.wait_exit(timeout_ms=10_000)
            assert res["exited"] and res["exit_code"] == 0, res
            t_branch_b = time.perf_counter()
            print(
                f"[Branch B] {conv_b.line} ({t_branch_b - t_restore:.2f}s wall; "
                f"restore={t_restore - t_branch_a:.2f}s)",
                flush=True,
            )

            fin = c.finish()
            print(
                f"recorded cluster {fin['id']} ({fin['verdict']}) "
                f"at {res['cluster_ns'] / 1e6:.1f}ms virtual time "
                f"(boot={t_boot - t0:.1f}s, branch_a={t_branch_a - t_snap:.1f}s, "
                f"branch_b={t_branch_b - t_restore:.1f}s)"
            )
        except Exception:
            for h in [*node_handles, client]:
                all_out = h.logs(stream="output")
                errs = [l for l in all_out if "ERROR" in l or "Exception" in l or "WARN" in l][-15:]
                out = all_out[-25:]
                print(f"\n=== {h.name} errors/warns ===", file=sys.stderr)
                for line in errs:
                    print(line, file=sys.stderr)
                print(f"=== {h.name} output (tail) ===", file=sys.stderr)
                for line in out:
                    print(line, file=sys.stderr)
            raise


if __name__ == "__main__":
    main()
