# `nix flake check`: everything CI asserts, offline.
{
  pkgs,
  rewind,
  kernel,
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

        # A fork reads its parent's keyframes up to its step and replays
        # like any run, from boot and from a keyframe on either side of the
        # step. So does a fork of that fork.
        id() { sed -n 's/.*"id":"\([0-9a-f]*\)".*/\1/p'; }
        manifest() { cat $REWIND_HOME/runs/$1/manifest.json; }
        fork=$(rewind fork a 400 --schedule 3 --json | id)
        manifest $fork | grep -q '"shared_keyframes"'
        manifest $fork | grep -q '"trace_hash"'
        rewind replay $fork | grep '^identical'
        rewind replay $fork --from 300 | grep '^identical'
        rewind replay $fork --from 999999 | grep '^identical'

        # A fork of the fork, halfway between the fork's step and its end,
        # is the fork exit for exit until its own step: it first differs
        # there or later, and restores from the keyframe the two share just
        # before it.
        last=$(rewind events $fork | tail -1 | awk '{print $1}')
        step=$(( (400 + last) / 2 ))
        fork2=$(rewind fork $fork $step --schedule 5 --json | id)
        differs=$(manifest $fork2 | sed -n 's/.*"first_difference": *\([0-9]*\).*/\1/p')
        echo "fork of a fork at $step first differs at ''${differs:-no step}"
        test -z "$differs" || test "$differs" -ge "$step"
        rewind replay $fork2 | grep '^identical'
        rewind replay $fork2 --from 300 | grep '^identical'
        rewind replay $fork2 --from $((step - 1)) | grep "^identical from the keyframe at step $((step - 1)) "
        rewind replay $fork2 --from 999999 | grep '^identical'

        # A replayable export of the fork of a fork carries every keyframe
        # it reads, and replays where neither parent is.
        rewind export --replayable -o fork2.rwd $fork2
        REWIND_HOME=$TMPDIR/elsewhere rewind import fork2.rwd
        REWIND_HOME=$TMPDIR/elsewhere rewind replay $fork2 --from 300 | grep '^identical'

        # Without its parent, a fork says which run it needs.
        mv $REWIND_HOME/runs/$fork $TMPDIR/away
        ! rewind replay $fork2 --from 300 2> missing
        grep "$fork" missing
        mv $TMPDIR/away $REWIND_HOME/runs/$fork

        # Forks past the end of the run are the run itself: prune removes
        # them and keeps the run and the fork that ran differently.
        same1=$(rewind fork a 999999 --schedule 7 --json | id)
        same2=$(rewind fork a 999999 --schedule 8 --json | id)
        ! manifest $same1 | grep -q '"first_difference"'
        manifest $fork | grep -q '"first_difference"'
        rewind prune a --identical --dry-run --json | tee planned
        grep -q "$same1" planned
        grep -q "$same2" planned
        ! grep -q "$fork" planned
        test -d $REWIND_HOME/runs/$same1
        rewind prune a --identical
        test ! -e $REWIND_HOME/runs/$same1
        test ! -e $REWIND_HOME/runs/$same2
        test -d $REWIND_HOME/runs/$fork
        rewind replay $fork --from 300 | grep '^identical'
        touch $out
      '';

  # checks.inspect: looking inside a recorded run at a step. A file read
  # before and after it is written, a shell that reads it, and gdb stopping
  # at a breakpoint in the VM's kernel. Boots the VM, so it needs /dev/kvm.
  inspect =
    pkgs.runCommand "rewind-inspect"
      {
        nativeBuildInputs = [
          rewind
          pkgs.gdb
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        rewind run -q --name w --root ${busyboxRoot} -- \
          sh -c 'echo first > /notes.txt; sleep 1; echo second >> /notes.txt'
        start=$(rewind events w | grep 'rewind-start' | head -1 | awk '{print $1}')

        # rewind cat: missing before the job, both lines at the end.
        status=0
        rewind cat w "$start" /notes.txt || status=$?
        test "$status" = 2
        rewind cat w 999999 /notes.txt | tee cat
        grep -q second cat

        # rewind shell, typed into from a pipe.
        printf 'cat /notes.txt; exit\n' | rewind shell w 999999 | tee shell
        grep -q second shell

        # rewind gdb: a breakpoint where every event is reported.
        rewind gdb w "$start" --listen 127.0.0.1:12345 &
        sleep 1
        gdb -q -batch -ex 'target remote 127.0.0.1:12345' \
          -ex 'break rewind_emit' -ex continue -ex detach \
          ${kernel}/vmlinux | tee gdb
        wait
        grep -q 'in rewind_emit' gdb

        # rewind gdb at a step inside the job: it names the process that
        # was running and loads its program, which in an image job comes
        # from the VM.
        write=$(rewind events w | grep 'notes.txt' | head -1)
        step=$(echo "$write" | awk '{print $1}')
        pid=$(echo "$write" | awk '{print $2}' | cut -d/ -f1)
        rewind gdb w "$step" --listen 127.0.0.1:12346 2> serve &
        while ! grep -q 'connect with' serve; do sleep 0.1; done
        gdb -q -batch -ex 'target remote 127.0.0.1:12346' -ex detach
        wait
        cat serve
        grep -q "ran in process $pid; loading symbols for 1 of its files" serve
        touch $out
      '';

  # checks.pmu-boot: `rewind pmu enable` as the NixOS module's boot service
  # runs it, with no HOME or anything else in its environment. Whether the
  # workaround can be set depends on the machine, so this only checks that
  # the command gets as far as trying.
  pmu-boot = pkgs.runCommand "rewind-pmu-boot" { nativeBuildInputs = [ rewind ]; } ''
    env -i ${pkgs.lib.getExe rewind} pmu enable > out 2>&1 || true
    cat out
    ! grep -q 'HOME' out
    touch $out
  '';

  # checks.version: VERSION is the release's version, and Cargo cannot read
  # it, so the two Cargo.toml files that name it must agree with it;
  # tools/set-version changes all three.
  version =
    let
      inherit (pkgs) lib;
      release = lib.fileContents ../VERSION;
      engine = (lib.importTOML ../Cargo.toml).workspace.package.version;
      app = (lib.importTOML ../crates/rewind-app/Cargo.toml).package.version;
    in
    assert lib.assertMsg (engine == release) "Cargo.toml says ${engine}, VERSION says ${release}";
    assert lib.assertMsg (
      app == release
    ) "crates/rewind-app/Cargo.toml says ${app}, VERSION says ${release}";
    pkgs.writeText "rewind-version" release;

  # checks.module: the NixOS module installs both packages, adds Rewind's
  # binary cache and sets the AMD workaround at boot. Only evaluates, so
  # it needs no KVM.
  module =
    let
      config = moduleSystem.config;
      installed = map (p: p.name) config.environment.systemPackages;
    in
    assert builtins.elem rewind.name installed;
    assert builtins.any (name: pkgs.lib.hasPrefix "rewind-app-" name) installed;
    assert builtins.elem "msr" config.boot.kernelModules;
    assert builtins.elem "https://rewindvm.cachix.org" config.nix.settings.extra-substituters;
    pkgs.writeText "rewind-module" config.systemd.services.rewind-pmu.serviceConfig.ExecStart;
}
