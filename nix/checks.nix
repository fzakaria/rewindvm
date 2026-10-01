# `nix flake check`: everything CI asserts, offline.
{
  pkgs,
  rewind,
  module,
}:
let
  # A NixOS system with every option of the module turned on, evaluated
  # but not built.
  moduleSystem = import "${pkgs.path}/nixos/lib/eval-config.nix" {
    inherit (pkgs.stdenv.hostPlatform) system;
    modules = [
      module
      {
        programs.rewind.enable = true;
        programs.rewind.amdBranchCounterWorkaround = true;
        programs.rewind.app.enable = true;
        boot.loader.grub.enable = false;
        fileSystems."/" = {
          device = "none";
          fsType = "tmpfs";
        };
        system.stateVersion = "25.11";
      }
    ];
  };

  # A root filesystem of static busybox, for the determinism check.
  busyboxRoot = pkgs.runCommand "rewind-busybox-root" { } ''
    mkdir -p $out/bin
    cp ${pkgs.pkgsStatic.busybox}/bin/busybox $out/bin/
    for tool in sh echo cat head od sleep seq; do
      ln -s busybox $out/bin/$tool
    done
  '';

  # Background jobs racing through a pipe, the kernel's RNG, and a sleep:
  # everything that would differ between two runs of an ordinary VM.
  workload = ''
    echo "uuid $(cat /proc/sys/kernel/random/uuid)"
    echo "random $(head -c 16 /dev/urandom | od -An -tx1)"
    for i in $(seq 8); do (echo "job $i") & done | cat
    wait
    sleep 1
    echo "uptime $(cat /proc/uptime)"
  '';
in
{
  # House style, checked rather than remembered: no em dashes and none of
  # the vocabulary that marks machine written prose, in docs, site copy and
  # code comments alike. Runs over a copy of the whole tree.
  prose = pkgs.runCommand "rewind-prose" { nativeBuildInputs = [ pkgs.python3 ]; } ''
    cp -r ${../.} tree
    chmod -R u+w tree
    python3 tree/tools/check-prose.py
    touch $out
  '';

  # The claim everything else rests on: a run replays exactly, from boot
  # and from a keyframe, and a different seed makes a different run. Needs
  # /dev/kvm in the build sandbox, which the kvm system feature provides.
  determinism =
    pkgs.runCommand "rewind-determinism"
      {
        nativeBuildInputs = [ rewind ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        rewind run -q --name a --root ${busyboxRoot} -- sh -c '${workload}'
        rewind replay a | tee replay
        grep -q '^identical' replay

        rewind replay a --from 400 | tee replay-from
        grep -q '^identical' replay-from

        rewind run -q --name b --seed 1 --root ${busyboxRoot} -- sh -c '${workload}'
        rewind diff a b | tee diff
        grep -q 'first difference' diff
        touch $out
      '';

  # checks.module: the NixOS module installs both packages and sets the
  # AMD workaround at boot. Only evaluates, so it needs no KVM.
  module =
    let
      config = moduleSystem.config;
      installed = map (p: p.name) config.environment.systemPackages;
    in
    assert builtins.elem rewind.name installed;
    assert builtins.elem "rewind-app-0.1.0" installed;
    assert builtins.elem "msr" config.boot.kernelModules;
    pkgs.writeText "rewind-module" config.systemd.services.rewind-pmu.serviceConfig.ExecStart;
}
