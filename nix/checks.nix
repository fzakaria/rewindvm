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
    for tool in sh echo cat head od sleep seq nproc grep taskset mkdir; do
      ln -s busybox $out/bin/$tool
    done
  '';

  # A root of two static programs for gdb, built with their symbols: one
  # prints, forks, and waits for its child; one writes a global twice.
  gdbRoot =
    pkgs.runCommand "rewind-gdb-root"
      {
        nativeBuildInputs = [ pkgs.pkgsStatic.stdenv.cc ];
        dontStrip = true;
      }
      ''
        mkdir -p $out/bin
        cat > fork.c <<'EOF'
        #include <sys/wait.h>
        #include <unistd.h>

        int main(void)
        {
          write(1, "start\n", 6);
          if (fork() == 0) {
            write(1, "child\n", 6);
            _exit(0);
          }
          wait(0);
          write(1, "parent\n", 7);
          return 0;
        }
        EOF
        $CC -static -O1 -g -o $out/bin/fork fork.c

        cat > watch.c <<'EOF'
        #include <unistd.h>

        volatile int counter;

        int main(void)
        {
          write(1, "start\n", 6);
          counter = 1;
          counter = 2;
          write(1, "done\n", 5);
          return 0;
        }
        EOF
        $CC -static -O1 -g -o $out/bin/watch watch.c
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

        # Removing a fork takes its forks too, and a leaf goes alone.
        leaf=$(rewind fork $fork2 $((step + 20)) --schedule 9 --json | id)
        rewind remove $leaf --json | tee removed
        grep -q "{\"removed\":\[\"$leaf\"\]}" removed
        test ! -e $REWIND_HOME/runs/$leaf

        # Running the fork's inputs again as a plain run leaves the fork
        # with no parent, still reading a's keyframes, so a stays.
        rewind run -q --schedule 3 --schedule-from 400 --root ${busyboxRoot} -- sh -c '${workload}'
        ! manifest $fork | grep -q '"parent": \['
        ! rewind remove a 2> refused
        cat refused
        grep -q "run $fork reads its keyframes up to step 399 from " refused
        rewind replay a --from 300 | grep '^identical'

        rewind remove $fork2 --dry-run | tee planned
        grep -q "would remove $fork2" planned
        test -d $REWIND_HOME/runs/$fork2
        rewind remove $fork --json | tee removed
        grep -q "{\"removed\":\[\"$fork\",\"$fork2\"\]}" removed
        test ! -e $REWIND_HOME/runs/$fork
        test ! -e $REWIND_HOME/runs/$fork2
        rewind replay a --from 300 | grep '^identical'
        touch $out
      '';

  # checks.cores: with --cores 4, the affinity system call busybox's nproc
  # reads, the sysfs list glibc reads and /proc/cpuinfo all say 4. Pinning to one of the four succeeds and to
  # a fifth fails, as on a machine with four. With the default, all say 1.
  # Boots the VM, so it needs /dev/kvm.
  cores =
    pkgs.runCommand "rewind-cores"
      {
        nativeBuildInputs = [ rewind ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        probe='
          echo "nproc $(nproc)"
          echo "online $(cat /sys/devices/system/cpu/online)"
          echo "cpuinfo $(grep -c ^processor /proc/cpuinfo)"
          taskset -c 3 echo "pinned 3" || echo "refused 3"
          taskset -c 4 echo "pinned 4" || echo "refused 4"
        '
        rewind run -q --name four --cores 4 --root ${busyboxRoot} -- sh -c "$probe"
        rewind log four | tee four
        grep -qx 'nproc 4' four
        grep -qx 'online 0-3' four
        grep -qx 'cpuinfo 4' four
        grep -qx 'pinned 3' four
        grep -qx 'refused 4' four

        rewind run -q --name one --root ${busyboxRoot} -- sh -c "$probe"
        rewind log one | tee one
        grep -qx 'nproc 1' one
        grep -qx 'online 0' one
        grep -qx 'cpuinfo 1' one
        grep -qx 'refused 3' one

        rewind replay four | grep '^identical'
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

  # checks.gdb-step: single-stepping in `rewind gdb` keeps the fork on the
  # recording, and gdb is stopped only where it asked. Two shells compute
  # side by side under a perturbed schedule, whose reschedules are
  # interrupts sent at exits; from every fourth step of the job, gdb steps
  # across some of them and continues to the end. Then gdb steps a program
  # past its fork system call and continues: the child starts with the
  # trap flag the step left in the flags it copied, which must not stop
  # gdb. Exit time and the schedule are chosen here so the check means the
  # same on any machine. Boots the VM, so it needs /dev/kvm.
  gdb-step =
    pkgs.runCommand "rewind-gdb-step"
      {
        nativeBuildInputs = [
          rewind
          pkgs.gdb
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        # No debuginfod server: without a network, each of gdb's questions
        # to it waits out a timeout.
        export REWIND_DEBUGINFOD=/nonexistent

        # A fork's records, and stops only where gdb asked.
        follows() {
          if ! grep -q 'exited normally' gdb || grep -q 'left the recording\|SIGTRAP' gdb; then
            echo "$1:"
            cat gdb
            exit 1
          fi
        }

        rewind run -q --clock exits --schedule 5 --name busy --root ${busyboxRoot} -- \
          sh -c 'for i in 1 2; do (n=0; while [ $n -lt 3000 ]; do n=$((n+1)); done; echo $i) & done; wait'
        start=$(rewind events busy | grep 'rewind-start' | head -1 | awk '{print $1}')
        end=$(rewind events busy | grep 'rewind-exit' | head -1 | awk '{print $1}')

        for step in $(seq "$start" 4 "$end"); do
          rewind gdb busy "$step" -- -batch -ex 'stepi 3000' -ex continue > gdb 2>&1 || true
          follows "stepping from step $step"
        done

        rewind run -q --clock exits --name fork --root ${gdbRoot} -- /bin/fork
        start=$(rewind events fork | grep 'write(1, "start' | awk '{print $1}')
        rewind gdb fork "$start" -- -batch \
          -ex 'break _Fork' -ex continue -ex 'stepi 300' -ex continue > gdb 2>&1 || true
        follows "stepping past fork"
        touch $out
      '';

  # checks.gdb-watch: `watch` in `rewind gdb` is a debug register: gdb
  # stops just after each write to a program's global, with its old and
  # new values, and the fork runs on to the end on the recording. x86 has
  # no trap on reads alone, so `rwatch` is refused. Boots the VM, so it
  # needs /dev/kvm.
  gdb-watch =
    pkgs.runCommand "rewind-gdb-watch"
      {
        nativeBuildInputs = [
          rewind
          pkgs.gdb
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        # No debuginfod server: without a network, each of gdb's questions
        # to it waits out a timeout.
        export REWIND_DEBUGINFOD=/nonexistent

        rewind run -q --clock exits --name watch --root ${gdbRoot} -- /bin/watch
        start=$(rewind events watch | grep 'write(1, "start' | awk '{print $1}')

        rewind gdb watch "$start" -- -batch \
          -ex 'watch counter' -ex continue -ex continue -ex continue > gdb 2>&1 || true
        cat gdb
        grep -q 'Old value = 0' gdb
        grep -q 'New value = 1' gdb
        grep -q 'New value = 2' gdb
        grep -q 'exited normally' gdb
        ! grep -q 'left the recording\|SIGTRAP' gdb

        rewind gdb watch "$start" -- -batch -ex 'rwatch counter' -ex continue > read 2>&1 || true
        cat read
        grep -q 'Could not insert' read
        touch $out
      '';

  # checks.search: `rewind check` starts each run it tries at the
  # unperturbed run's latest keyframe before the run's schedule starts,
  # not at boot. Every one of them still replays from boot to the same
  # trace, and none reads the unperturbed run's keyframes afterwards, so
  # removing that run leaves them whole. Four background jobs race to
  # write a file first; on some CPUs, some schedules change which one wins.
  # Boots the VM, so it needs /dev/kvm.
  search =
    pkgs.runCommand "rewind-search"
      {
        nativeBuildInputs = [
          rewind
          pkgs.jq
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        race='
          mkdir -p /run/race
          for i in 1 2 3 4; do (echo $i >> /run/race/o) & done
          wait
          test "$(head -1 /run/race/o)" = 1
        '
        # check exits 1 when a schedule ends differently and 0 when none
        # does. Which depends on the CPU vendor, since runs differ between
        # them: on AMD a schedule changes which job wins, and on Intel none
        # of the 64 has. Either way, every run check made must replay.
        if rewind check -j 4 --root ${busyboxRoot} -- sh -c "$race" > found; then
          cat found
          grep -q 'same result under all' found
        else
          cat found
          grep -q 'still ends differently' found
        fi
        for dir in $REWIND_HOME/runs/*; do
          id=$(basename $dir)
          jq -e '.shared_keyframes == null' $dir/manifest.json > /dev/null
          rewind replay $id | grep '^identical'
        done
        touch $out
      '';

  # checks.yield: a thread that calls sched_yield until another thread sets
  # a flag finishes under every schedule. A yield makes an exit, so virtual
  # time moves and the other thread gets to run; before, a perturbed
  # schedule that switched to the yielding thread first ran forever. The
  # timeout turns that hang into a failure. Boots the VM, so it needs
  # /dev/kvm.
  yield =
    let
      source = pkgs.writeText "yieldspin.c" ''
        #include <pthread.h>
        #include <sched.h>
        #include <stdatomic.h>
        #include <stdio.h>
        #include <unistd.h>

        static atomic_int flag;

        /* Each write is an exit, where a perturbed schedule may switch to
           the yielding thread before the flag is set. */
        static void *setter(void *arg) {
            (void)arg;
            for (int i = 0; i < 20; i++) {
                write(2, ".", 1);
            }
            atomic_store(&flag, 1);
            return NULL;
        }

        int main(void) {
            pthread_t t;
            pthread_create(&t, NULL, setter, NULL);
            while (!atomic_load(&flag)) {
                sched_yield();
            }
            pthread_join(t, NULL);
            puts("done");
            return 0;
        }
      '';
      root = pkgs.pkgsStatic.runCommandCC "rewind-yield-root" { } ''
        mkdir -p $out/bin
        $CC -static -O2 -pthread -o $out/bin/yieldspin ${source}
      '';
    in
    pkgs.runCommand "rewind-yield"
      {
        nativeBuildInputs = [
          rewind
          pkgs.coreutils
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        timeout 600 rewind check -j 4 --cores 2 --schedules 16 --root ${root} -- /bin/yieldspin | tee found
        grep -q 'same result under all 17 schedules' found
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
