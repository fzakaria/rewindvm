#!/usr/bin/env python3
"""Deterministic 3-node Apache Cassandra 6.0 (CEP-15 Accord) cluster test.

Boots three real Apache Cassandra 6.0-alpha2 VMs with Accord enabled
(`accord.enabled: true`, `storage_compatibility_mode: NONE`, `RF=3`) and slight
inter-node clock skew (`0ms`, `+15ms`, `-10ms`), plus a zero-dependency Python
CQLv4 driver node that exercises strict-serializable multi-partition Accord
transactions (`BEGIN TRANSACTION ... LET ... IF ... UPDATE ... COMMIT TRANSACTION`):

  Phase 1 — 3-node Accord fast path:
      Creates `bank.accounts` with `transactional_mode = 'full'`, seeds
      alice/bob/carol = 1000 each (across 3 partitions), runs multi-partition
      conditional transfers rotating coordinators across all 3 nodes, and
      verifies strict conservation of money (`total == 3000`, no overdrafts).

  Phase 2 — Minority partition (Accord slow-path quorum):
      Snapshots the live cluster, isolates `node3` from `[node1, node2, client]`,
      and verifies multi-partition Accord transactions continue committing on
      the 2-node quorum while preserving `total == 3000`.

  Phase 3 — Partition heal & linearizable recovery:
      Heals `node3`, waits for catch-up, and runs an Accord read transaction
      directly on `node3` to verify it observes the exact committed state.

Requires `nix` on PATH (for `.#cassandra6` and `nixpkgs#python3`). Run with:
    python3 examples/cluster/cassandra_accord.py

Or explore perturbed schedules/nemesis faults automatically:
    rewind cluster check --schedules 4 --nemesis -- python3 examples/cluster/cassandra_accord.py
"""

import os
import sys
import textwrap
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "sdk" / "python"))

from rewind_cluster import Cluster

REPO_ROOT = Path(__file__).resolve().parents[2]

NODES = [
    ("node1", "10.0.0.1", "-9223372036854775808", 0),
    ("node2", "10.0.0.2", "-3074457345618258603", 15),
    ("node3", "10.0.0.3", "3074457345618258602", -10),
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


# Embedded stdlib-only CQL native protocol v4 client + Accord invariant checker.
DRIVER_SCRIPT = r'''
import socket
import struct
import sys
import time

NODES = ["10.0.0.1", "10.0.0.2", "10.0.0.3"]

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
        # String map: {"CQL_VERSION": "3.0.0"}
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
        if tid == 0x0000:  # custom
            _, off = self._read_short_str(buf, off)
        elif tid in (0x0020, 0x0022):  # list / set
            off = self._skip_type(buf, off)
        elif tid == 0x0021:  # map
            off = self._skip_type(buf, off)
            off = self._skip_type(buf, off)
        elif tid == 0x0030:  # UDT
            _, off = self._read_short_str(buf, off)
            _, off = self._read_short_str(buf, off)
            n = struct.unpack_from("!H", buf, off)[0]
            off += 2
            for _ in range(n):
                _, off = self._read_short_str(buf, off)
                off = self._skip_type(buf, off)
        elif tid == 0x0031:  # tuple
            n = struct.unpack_from("!H", buf, off)[0]
            off += 2
            for _ in range(n):
                off = self._skip_type(buf, off)
        return off

    def _parse_rows(self, buf: bytes, off: int) -> list[list[bytes | None]]:
        flags, col_count = struct.unpack_from("!ii", buf, off)
        off += 8
        if flags & 0x0002:  # Has_more_pages
            plen = struct.unpack_from("!i", buf, off)[0]
            off += 4 + max(plen, 0)
        if not (flags & 0x0004):  # Metadata present
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


def connect_with_retry(host: str, deadline: float) -> CqlConn:
    last_err = None
    while time.monotonic() < deadline:
        try:
            return CqlConn(host, timeout=15.0)
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


def accord_transfer(conn: CqlConn, src: str, dst: str, amount: int) -> None:
    """Strictly serializable multi-partition Accord conditional transfer."""
    cql = f"""
    BEGIN TRANSACTION
      LET s = (SELECT balance FROM bank.accounts WHERE id = '{src}');
      LET d = (SELECT balance FROM bank.accounts WHERE id = '{dst}');
      IF s.balance >= {amount} THEN
        UPDATE bank.accounts SET balance -= {amount} WHERE id = '{src}';
        UPDATE bank.accounts SET balance += {amount} WHERE id = '{dst}';
      END IF
    COMMIT TRANSACTION
    """
    conn.query(cql, CL_QUORUM)


def check_invariant(label: str, balances: dict[str, int]) -> None:
    total = sum(balances.values())
    print(f"{label}: balances={balances} total={total}", flush=True)
    for acct, bal in balances.items():
        if bal < 0:
            raise AssertionError(f"OVERDRAFT on {acct}: {balances}")
    if total != 3000:
        raise AssertionError(f"CONSERVATION VIOLATION: total={total} balances={balances}")


def run_with_retry(fn, attempts: int = 8, delay: float = 0.5):
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
    # Seed 3 distinct partitions via Accord transaction.
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

    # Phase 1: 3-node Accord fast-path transfers rotating across all 3 coordinators.
    phase1_transfers = [
        (c1, "alice", "bob", 150),
        (c2, "bob", "carol", 200),
        (c3, "carol", "alice", 75),
        (c1, "alice", "carol", 125),
    ]
    for conn, src, dst, amt in phase1_transfers:
        run_with_retry(lambda c=conn, s=src, d=dst, a=amt: accord_transfer(c, s, d, a))
    b1 = run_with_retry(lambda: accord_read_all(c3))
    check_invariant("PHASE1", b1)
    assert b1 == {"alice": 800, "bob": 950, "carol": 1250}, b1
    print("PHASE1_OK", flush=True)

    # Give the host orchestrator a clean virtual-time window to snapshot & partition node3.
    time.sleep(0.5)

    # Phase 2: node3 is partitioned away — Accord must still commit via 2-node quorum!
    phase2_transfers = [
        (c1, "carol", "alice", 250),
        (c2, "bob", "alice", 50),
    ]
    for conn, src, dst, amt in phase2_transfers:
        run_with_retry(lambda c=conn, s=src, d=dst, a=amt: accord_transfer(c, s, d, a))
    b2 = run_with_retry(lambda: accord_read_all(c1))
    check_invariant("PHASE2", b2)
    assert b2 == {"alice": 1100, "bob": 900, "carol": 1000}, b2
    print("PHASE2_OK", flush=True)

    # Give the host orchestrator time to heal node3 and let gossip/Accord catch up.
    time.sleep(3.0)

    # Phase 3: Reconnect to healed node3 and verify linearizable Accord read.
    c3.close()
    c3 = connect_with_retry(NODES[2], time.monotonic() + 30.0)
    b3 = run_with_retry(lambda: accord_read_all(c3), attempts=15, delay=0.5)
    check_invariant("PHASE3_NODE3", b3)
    assert b3 == b2, f"node3 stale after heal: {b3} != {b2}"
    print("PHASE3_OK", flush=True)

    for c in (c1, c2, c3):
        c.close()
    print("ACCORD_ALL_OK", flush=True)


if __name__ == "__main__":
    main()
'''


def main() -> None:
    cassandra_pkg = os.environ.get("REWIND_CASSANDRA_PKG", f"{REPO_ROOT}#cassandra6")
    seeds = "10.0.0.1:7000"

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
        for idx, (name, ip, token, skew_ms) in enumerate(NODES):
            # Stagger joins cleanly (Cassandra CEP-21 TCM forbids concurrent
            # bootstraps): node2 waits for node1:9042, node3 waits for node2:9042.
            if idx == 0:
                cmd = ["cassandra-node", "/build", "/build/data"]
            else:
                prev_ip = NODES[idx - 1][1]
                cmd = [
                    "/bin/sh",
                    "-c",
                    f"until nc -z {prev_ip} 9042 2>/dev/null; do sleep 0.5; done; "
                    "exec cassandra-node /build /build/data",
                ]
            h = c.add_node(
                name,
                cmd,
                ip=ip,
                role="service",
                packages=[cassandra_pkg],
                mem_mib=1024,
                clock_skew_ms=skew_ms,
                files={
                    "/build/cassandra.yaml": cassandra_yaml(ip, token, seeds),
                    "/build/logback.xml": LOGBACK_XML,
                },
            )
            node_handles.append(h)

        client = c.add_node(
            "client",
            ["python3", "-u", "/build/driver.py"],
            ip="10.0.0.4",
            role="driver",
            packages=["nixpkgs#python3"],
            mem_mib=256,
            files={"/build/driver.py": DRIVER_SCRIPT},
        )

        c.start()
        try:
            # Wait for all 3 Cassandra 6.0 nodes to open CQL native transport.
            for h in node_handles:
                h.wait_for(
                    "Starting listening for CQL clients",
                    from_start=True,
                    timeout_ms=120_000,
                )

            client.wait_for("CLUSTER_3_NODES_JOINED", from_start=True, timeout_ms=60_000)
            client.wait_for("PHASE1_OK", from_start=True, timeout_ms=60_000)

            # Snapshot pre-partition cluster state, then isolate node3 from quorum.
            c.snapshot("before-partition")
            c.partition(["node3"], ["node1", "node2", "client"])

            client.wait_for("PHASE2_OK", from_start=True, timeout_ms=60_000)

            # Heal node3 and verify linearizable Accord recovery.
            c.heal()
            client.wait_for("PHASE3_OK", from_start=True, timeout_ms=60_000)
            client.wait_for("ACCORD_ALL_OK", from_start=True, timeout_ms=30_000)

            res = client.wait_exit(timeout_ms=10_000)
            assert res["exited"] and res["exit_code"] == 0, res
            fin = c.finish()
            print(
                f"recorded cluster {fin['id']} ({fin['verdict']}) "
                f"at {res['cluster_ns'] / 1e6:.1f}ms virtual time"
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
