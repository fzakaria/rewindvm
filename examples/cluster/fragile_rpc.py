#!/usr/bin/env python3
"""Flaky 2-node RPC cluster for `rewind cluster check --nemesis -- python3 ...`.

Passes cleanly under schedule 0 (no nemesis). Under `--nemesis`, the naive
client without retries fails on a transient partition/crash, and `rewind
cluster check` automatically bisects the deciding virtual-time fault window!
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "sdk" / "python"))

from rewind_cluster import Cluster, Nemesis


def main() -> None:
    with Cluster(
        "fragile-rpc",
        latency_ms=2,
        jitter_us=200,
        mem_mib=128,
        stop_when="driver",
        max_virtual_ms=10_000,
        nemesis=Nemesis(
            tick_ms=100,
            partition_prob_ppm=250_000,
            heal_prob_ppm=250_000,
            crash_prob_ppm=150_000,
            restart_prob_ppm=300_000,
            max_crashed=1,
        ),
    ) as c:
        server = c.add_node(
            "server",
            "echo 'SERVER_READY'; while true; do /rewind/tools/bin/nc -l -p 7001 -e /bin/sh -c 'echo ok'; done",
            ip="10.0.0.1",
            role="service",
        )
        client = c.add_node(
            "client",
            """
set -e
sleep 0.3
for i in 1 2 3 4 5; do
  resp=$(/rewind/tools/bin/nc -w 1 server 7001)
  echo "rpc $i -> $resp"
  [ "$resp" = "ok" ]
  sleep 0.2
done
echo "CLIENT_OK"
""",
            ip="10.0.0.2",
            role="driver",
        )

        c.start()
        server.wait_for("SERVER_READY")
        res = client.wait_exit(timeout_ms=10_000)
        assert res["exited"] and res["exit_code"] == 0, f"client failed: {res}"
        client.wait_for("CLIENT_OK", from_start=True)


if __name__ == "__main__":
    main()
