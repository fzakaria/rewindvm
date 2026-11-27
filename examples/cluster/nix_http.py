#!/usr/bin/env python3
"""Multi-node cluster running real Nix packages (`nixpkgs#python3`, `nixpkgs#curl`).

Each node's Nix closure is resolved once, packed into a cached EROFS store
image under `$REWIND_HOME/images/nix-<hash>.erofs`, mounted at `/nix/store`,
and each package's `bin/` directory is automatically prepended to `PATH`.

Requires `nix` on PATH. Run with:
    python3 examples/cluster/nix_http.py
"""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "sdk" / "python"))

from rewind_cluster import Cluster


def main() -> None:
    with Cluster(
        "nix-http-demo",
        latency_ms=2,
        jitter_us=200,
        mem_mib=256,
        stop_when="driver",
        max_virtual_ms=20_000,
    ) as c:
        srv = c.add_node(
            "api",
            "echo 'hello-from-nix' > /build/greeting.txt && "
            "echo 'API_READY' && "
            "python3 -m http.server 8000 --directory /build",
            ip="10.0.0.1",
            role="service",
            packages=["nixpkgs#python3"],
        )
        cli = c.add_node(
            "client",
            """
set -e
sleep 0.3
echo "R1=$(curl -sf --max-time 1 http://api:8000/greeting.txt || echo blocked)"
sleep 0.5
echo "R2=$(curl -sf --max-time 1 http://api:8000/greeting.txt || echo blocked)"
sleep 0.5
echo "R3=$(curl -sf --max-time 1 http://api:8000/greeting.txt || echo blocked)"
echo "NIX_HTTP_OK"
""",
            ip="10.0.0.2",
            role="driver",
            packages=["nixpkgs#curl"],
        )

        c.start()
        srv.wait_for("API_READY")

        r1 = cli.wait_for("R1=")
        assert r1.line == "R1=hello-from-nix", r1.line
        assert srv.cat("/build/greeting.txt").strip() == "hello-from-nix"

        c.partition(["api"], ["client"])
        r2 = cli.wait_for("R2=")
        assert r2.line == "R2=blocked", r2.line

        c.heal()
        r3 = cli.wait_for("R3=")
        assert r3.line == "R3=hello-from-nix", r3.line

        cli.wait_for("NIX_HTTP_OK")
        res = cli.wait_exit()
        assert res["exited"] and res["exit_code"] == 0, res
        fin = c.finish()
        print(f"recorded cluster {fin['id']} ({fin['verdict']})")


if __name__ == "__main__":
    main()
