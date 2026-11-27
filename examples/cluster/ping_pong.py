#!/usr/bin/env python3
"""Reactive 3-node distributed TCP cluster driven by the Rewind Python SDK.

Demonstrates:
- Reactive waiting on guest stdout (`wait_for`) instead of hardcoded sleeps
- Live zero-perturbation filesystem inspection (`cat` / `files`)
- Timeline snapshotting and branching (`snapshot` / `restore`)
- Dynamic partitions, heals, link latency updates, and hard node crash/reboot
- Standalone deterministic replay (`rewind cluster replay <id>`) without Python!
"""

import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "sdk" / "python"))

from rewind_cluster import Cluster


def main() -> None:
    with Cluster(
        "ping-pong-sdk",
        latency_ms=2,
        jitter_us=200,
        mem_mib=128,
        stop_when="driver",
        max_virtual_ms=20_000,
        echo=os.environ.get("REWIND_ECHO") == "1",
    ) as c:
        n1 = c.add_node(
            "node1",
            "echo 'hello-from-node1' > /tmp/reply; echo 'READY node1'; "
            "while true; do /rewind/tools/bin/nc -l -p 7000 -e /bin/sh -c 'cat /tmp/reply'; done",
            ip="10.0.0.1",
            role="service",
        )
        n2 = c.add_node(
            "node2",
            "echo 'hello-from-node2' > /tmp/reply; echo 'READY node2'; "
            "while true; do /rewind/tools/bin/nc -l -p 7000 -e /bin/sh -c 'cat /tmp/reply'; done",
            ip="10.0.0.2",
            role="service",
            clock_skew_ms=25,
            clock_drift_ppm=500,
        )
        cli = c.add_node(
            "client",
            """
set -e
probe() {
  /rewind/tools/bin/nc -w 1 "$1" 7000 2>/dev/null || echo "blocked"
}
echo "CLIENT_START"
sleep 0.2
echo "PHASE1 n1=$(probe node1) n2=$(probe node2)"
sleep 0.6
echo "PHASE2 n1=$(probe node1) n2=$(probe node2)"
sleep 0.6
echo "PHASE3 n1=$(probe node1) n2=$(probe node2)"
echo "ALL_CHECKS_PASSED"
""",
            ip="10.0.0.10",
            role="driver",
        )

        c.start()

        # 1. Reactively wait for both servers and phase 1 completion.
        n1.wait_for("READY node1")
        n2.wait_for("READY node2")
        p1 = cli.wait_for("PHASE1")
        assert p1.line == "PHASE1 n1=hello-from-node1 n2=hello-from-node2", p1.line

        # 2. Zero-perturbation live guest file inspection.
        assert n1.cat("/tmp/reply").strip() == "hello-from-node1"
        assert any(e["path"] == "reply" for e in n1.files("/tmp"))

        # 3. Snapshot before injecting faults so we can test branching!
        snap = c.snapshot("after-phase1")

        # Branch A: isolate node2 instead of node1, verify node2 is blocked.
        n2.isolate()
        p2_a = cli.wait_for("PHASE2")
        assert p2_a.line == "PHASE2 n1=hello-from-node1 n2=blocked", p2_a.line

        # Rewind back to `after-phase1` and explore Branch B!
        c.restore(snap)

        # Branch B: isolate node1 + add extra latency on client <-> node2.
        c.partition(["node1"], ["node2", "client"])
        c.set_link("client", "node2", latency_ms=10)
        p2_b = cli.wait_for("PHASE2")
        assert p2_b.line == "PHASE2 n1=blocked n2=hello-from-node2", p2_b.line

        # Heal partition and crash + cold-reboot node2 before phase 3.
        c.heal()
        n2.crash()
        c.advance(ms=150)
        n2.restart()
        n2.wait_for("READY node2")

        p3 = cli.wait_for("PHASE3")
        assert p3.line == "PHASE3 n1=hello-from-node1 n2=hello-from-node2", p3.line
        cli.wait_for("ALL_CHECKS_PASSED")
        exit_res = cli.wait_exit()
        assert exit_res["exited"] and exit_res["exit_code"] == 0, exit_res

        res = c.finish()
        print(
            f"recorded cluster {res['id']} ({res['verdict']}) "
            f"at {res['outcome']['cluster_ns'] / 1e6:.2f}ms"
        )


if __name__ == "__main__":
    main()
