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

  # A root of six static programs for gdb, built with their symbols: one
  # prints, forks, and waits for its child; one forks a child that prints
  # and writes a global, then writes the same global twice itself; one
  # forks a child that sleeps before writing to a pipe, prints, and blocks
  # reading the pipe; one starts three threads that block on a futex until
  # the main thread, after a sleep, prints and wakes them; one starts a
  # thread that prints and faults at once while the main thread waits on
  # a vfork child; one reads the clock, which musl does in the vDSO, after
  # each of two prints; one forks a child that calls a function three times
  # and runs an int3 of its own, then calls the same function and six
  # others itself.
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
        #include <sys/wait.h>
        #include <unistd.h>

        volatile int counter;

        int main(void)
        {
          write(1, "start\n", 6);
          if (fork() == 0) {
            write(1, "child\n", 6);
            counter = 1;
            _exit(0);
          }
          wait(0);
          counter = 2;
          counter = 3;
          write(1, "done\n", 5);
          return 0;
        }
        EOF
        $CC -static -O1 -g -o $out/bin/watch watch.c

        cat > block.c <<'EOF'
        #include <sys/wait.h>
        #include <unistd.h>

        int main(void)
        {
          int p[2];
          char c;

          pipe(p);
          if (fork() == 0) {
            sleep(1);
            write(p[1], "x", 1);
            _exit(0);
          }
          write(1, "wait\n", 5);
          read(p[0], &c, 1);
          wait(0);
          return 0;
        }
        EOF
        $CC -static -O1 -g -o $out/bin/block block.c

        cat > threads.c <<'EOF'
        #include <pthread.h>
        #include <sys/syscall.h>
        #include <unistd.h>
        #include <linux/futex.h>

        #define WORKERS 3

        static int gate;

        /* The futex system call made here rather than in musl, which has
           no unwind tables, so gdb finds each waiting worker in this file. */
        static void futex(int op, int val)
        {
          register long timeout __asm__("r10") = 0;
          long ret;

          __asm__ volatile("syscall"
                           : "=a"(ret)
                           : "a"(SYS_futex), "D"(&gate), "S"(op), "d"(val), "r"(timeout)
                           : "rcx", "r11", "memory");
        }

        static void *worker(void *arg)
        {
          while (!__atomic_load_n(&gate, __ATOMIC_ACQUIRE))
            futex(FUTEX_WAIT, 0);
          return arg;
        }

        int main(void)
        {
          pthread_t t[WORKERS];
          int i;

          for (i = 0; i < WORKERS; i++)
            pthread_create(&t[i], 0, worker, 0);
          sleep(1);
          write(1, "open\n", 5);
          __atomic_store_n(&gate, 1, __ATOMIC_RELEASE);
          futex(FUTEX_WAKE, WORKERS);
          for (i = 0; i < WORKERS; i++)
            pthread_join(t[i], 0);
          return 0;
        }
        EOF
        $CC -static -O1 -g -pthread -o $out/bin/threads threads.c

        cat > crash.c <<'EOF'
        #define _GNU_SOURCE
        #include <linux/futex.h>
        #include <pthread.h>
        #include <sys/syscall.h>
        #include <time.h>
        #include <unistd.h>

        static int go;

        /* Waits for the vfork child's word, then prints and faults at once:
           the print is its last system call. */
        static void *worker(void *arg)
        {
          while (!__atomic_load_n(&go, __ATOMIC_ACQUIRE))
            syscall(SYS_futex, &go, FUTEX_WAIT, 0, 0, 0, 0);
          write(1, "dying\n", 6);
          *(volatile int *)0 = 1;
          return arg;
        }

        int main(void)
        {
          pthread_t t;
          struct timespec second = { 1, 0 };

          pthread_create(&t, 0, worker, 0);

          /* The main thread waits for the vfork child in a sleep that no
             stop signal wakes, so only a signal to the worker stops it. */
          if (vfork() == 0) {
            __atomic_store_n(&go, 1, __ATOMIC_RELEASE);
            syscall(SYS_futex, &go, FUTEX_WAKE, 1, 0, 0, 0);
            syscall(SYS_nanosleep, &second, 0);
            syscall(SYS_exit, 0);
          }
          pthread_join(t, 0);
          return 0;
        }
        EOF
        $CC -static -O1 -g -pthread -o $out/bin/crash crash.c

        cat > vdso.c <<'EOF'
        #include <time.h>
        #include <unistd.h>

        int main(void)
        {
          struct timespec now;

          write(1, "start\n", 6);
          clock_gettime(CLOCK_MONOTONIC, &now);
          write(1, "again\n", 6);
          clock_gettime(CLOCK_MONOTONIC, &now);
          return 0;
        }
        EOF
        $CC -static -O1 -g -o $out/bin/vdso vdso.c

        cat > int3.c <<'EOF'
        #include <signal.h>
        #include <sys/wait.h>
        #include <unistd.h>

        volatile int calls;

        /* Calls for gdb to break on, each kept out of line. */
        __attribute__((noinline)) void first(void) { calls += 1; }
        __attribute__((noinline)) void second(void) { calls += 2; }
        __attribute__((noinline)) void third(void) { calls += 3; }
        __attribute__((noinline)) void fourth(void) { calls += 4; }
        __attribute__((noinline)) void fifth(void) { calls += 5; }
        __attribute__((noinline)) void sixth(void) { calls += 6; }

        /* Called by the child, then by the parent: one page of code. */
        __attribute__((noinline)) void shared(int who) { calls += who; }

        static void on_trap(int sig)
        {
          (void)sig;
          write(1, "trapped\n", 8);
        }

        int main(void)
        {
          signal(SIGTRAP, on_trap);
          write(1, "start\n", 6);
          if (fork() == 0) {
            for (int i = 0; i < 3; i++)
              shared(1);
            __asm__ volatile("int3");
            _exit(0);
          }
          wait(0);
          shared(0);
          first();
          second();
          third();
          fourth();
          fifth();
          sixth();
          write(1, "done\n", 5);
          return 0;
        }
        EOF
        $CC -static -O1 -g -o $out/bin/int3 int3.c
      '';

  # A root with a static program in two files, like mylib: main hands a
  # job to a pool whose worker thread prints it. The sources are in the
  # root under /src, the directory their DWARF names, so the VM has them.
  whereRoot =
    pkgs.runCommand "rewind-where-root"
      {
        nativeBuildInputs = [ pkgs.pkgsStatic.stdenv.cc ];
        dontStrip = true;
      }
      ''
        mkdir -p $out/bin $out/src
        cd $out/src
        cat > pool.h <<'EOF'
        struct pool {
          int job;
        };

        void pool_run(struct pool *p);
        EOF

        cat > pool.c <<'EOF'
        #include <pthread.h>
        #include <stdio.h>

        #include "pool.h"

        static void *worker(void *arg)
        {
          struct pool *p = arg;

          printf("worker picked job %d\n", p->job); /* the write */
          fflush(stdout);
          return 0;
        }

        void pool_run(struct pool *p)
        {
          pthread_t t;

          pthread_create(&t, 0, worker, p);
          pthread_join(t, 0); /* the wait */
        }
        EOF

        cat > main.c <<'EOF'
        #include "pool.h"

        int main(void)
        {
          struct pool p = { .job = 1 };

          pool_run(&p);
          return 0;
        }
        EOF
        $CC -static -O1 -g -pthread -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/pool main.c pool.c
      '';

  # A root with a static program whose two threads add to a total without
  # a lock: each reads it, writes a dot, which is an exit a schedule can
  # reschedule at, and stores what it read plus one. A reschedule between
  # the read and the store loses an add, and the program exits 1. The
  # source is in the root under /src, the directory its DWARF names.
  raceRoot =
    pkgs.runCommand "rewind-race-root"
      {
        nativeBuildInputs = [ pkgs.pkgsStatic.stdenv.cc ];
        dontStrip = true;
      }
      ''
        mkdir -p $out/bin $out/src
        cd $out/src
        cat > race.c <<'EOF'
        #include <pthread.h>
        #include <stdio.h>
        #include <unistd.h>

        #define WORKERS 2
        #define ADDS 10

        static long total;

        static void *add(void *arg)
        {
          for (int i = 0; i < ADDS; i++) {
            long seen = total;
            write(1, ".", 1); /* the exit */
            total = seen + 1;
          }
          return 0;
        }

        int main(void)
        {
          pthread_t t[WORKERS];

          for (int i = 0; i < WORKERS; i++)
            pthread_create(&t[i], 0, add, 0);
          for (int i = 0; i < WORKERS; i++)
            pthread_join(t[i], 0);
          printf("\ntotal %ld\n", total);
          return total != WORKERS * ADDS;
        }
        EOF
        $CC -static -O1 -g -pthread -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/race race.c
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

        # Running a run's inputs again replaces its recording only once the
        # new execution finishes: one killed partway, as by Ctrl-C, leaves
        # the trace and the manifest byte for byte as they were. 200,000
        # writes take a few seconds, long enough to kill.
        writes='for i in $(seq 200000); do echo $i; done'
        rewind run -q --name many --root ${busyboxRoot} -- sh -c "$writes"
        many=$(dirname "$(grep -l '"name": "many"' $REWIND_HOME/runs/*/manifest.json)")
        cp $many/trace.bin $many/manifest.json $TMPDIR/
        status=0
        timeout -s INT 1 rewind run -q --name many --root ${busyboxRoot} -- sh -c "$writes" || status=$?
        test $status = 124
        cmp $many/trace.bin $TMPDIR/trace.bin
        cmp $many/manifest.json $TMPDIR/manifest.json

        # A look inside a run more than 512 steps past its nearest
        # keyframe keeps one at its step. A second look there keeps none,
        # since it restores that keyframe and replays nothing, and the run
        # replays from the kept keyframe like from any other. A run of
        # thousands of writes has keyframes far enough apart.
        rewind run -q --name long --root ${busyboxRoot} -- sh -c 'for i in $(seq 3000); do echo $i; done'
        long=$(dirname "$(grep -l '"name": "long"' $REWIND_HOME/runs/*/manifest.json)")
        keyframes() { ls "$long/keyframes" | sed 's/\.kf$//; s/^0*//'; }
        end=$(rewind events long | grep 'mark "rewind-exit ' | awk '{print $1}')
        far=$( (keyframes; echo "$end") | sort -n | awk -v end="$end" \
          'NR > 1 && $1 - last > 600 && last + 600 < end { print last + 600; exit } { last = $1 }')
        echo "looking at step ''${far:-none} of long, keyframes: $(keyframes | tr '\n' ' ')"
        test -n "$far"
        count=$(keyframes | wc -l)
        rewind cat long "$far" /bin/busybox > /dev/null
        test "$(keyframes | wc -l)" = $((count + 1))
        keyframes > kept
        grep -qx "$far" kept
        rewind cat long "$far" /bin/busybox > /dev/null
        test "$(keyframes | wc -l)" = $((count + 1))
        rewind replay long --from "$far" | grep "^identical from the keyframe at step $far "

        rewind run -q --name b --seed 1 --root ${busyboxRoot} -- sh -c '${workload}'
        # rewind diff exits 1 for runs that differ, 0 for identical ones.
        status=0
        rewind diff a b > diff || status=$?
        cat diff
        test "$status" = 1
        grep -q 'first difference' diff
        rewind diff a a > same
        grep -qx identical same
        rewind diff a a --json > same.json
        grep -q '"divergence":null' same.json

        # A fork reads its parent's keyframes up to its step and replays
        # like any run, from boot and from a keyframe on either side of the
        # step. So does a fork of that fork.
        id() { sed -n 's/.*"id":"\([0-9a-f]*\)".*/\1/p'; }
        manifest() { echo "$REWIND_HOME/runs/$1/manifest.json"; }
        # The step a run ended at, the run named by its id or its name.
        end() { rewind ls --json | sed -n "s/.*\"\(id\|name\)\":\"$1\",.*\"steps\":\([0-9]*\).*/\2/p"; }
        fork=$(rewind fork a 400 --schedule 3 --json | id)
        grep -q '"shared_keyframes"' "$(manifest $fork)"
        grep -q '"trace_hash"' "$(manifest $fork)"
        rewind replay $fork | grep '^identical'
        rewind replay $fork --from 300 | grep '^identical'
        rewind replay $fork --from "$(end $fork)" | grep '^identical'

        # A fork of the fork, halfway between the fork's step and its end,
        # is the fork exit for exit until its own step: it first differs
        # there or later, and restores from the keyframe the two share just
        # before it.
        last=$(rewind events $fork | tail -1 | awk '{print $1}')
        step=$(( (400 + last) / 2 ))
        fork2=$(rewind fork $fork $step --schedule 5 --json | id)
        differs=$(sed -n 's/.*"first_difference": *\([0-9]*\).*/\1/p' "$(manifest $fork2)")
        echo "fork of a fork at $step first differs at ''${differs:-no step}"
        test -z "$differs" || test "$differs" -ge "$step"
        rewind replay $fork2 | grep '^identical'
        rewind replay $fork2 --from 300 | grep '^identical'
        rewind replay $fork2 --from $((step - 1)) | grep "^identical from the keyframe at step $((step - 1)) "
        rewind replay $fork2 --from "$(end $fork2)" | grep '^identical'

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

        # A fork past the end of the run is refused. Forks at its end are
        # the run itself: prune removes them and keeps the run and the fork
        # that ran differently.
        ! rewind fork a $(($(end a) + 1)) --schedule 7 2> past
        grep -q 'past its end' past
        same1=$(rewind fork a "$(end a)" --schedule 7 --json | id)
        same2=$(rewind fork a "$(end a)" --schedule 8 --json | id)
        ! grep -q '"first_difference"' "$(manifest $same1)"
        grep -q '"first_difference"' "$(manifest $fork)"
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

        # Two more leaves of the fork of a fork, removed together below.
        leaf1=$(rewind fork $fork2 $((step + 20)) --schedule 10 --json | id)
        leaf2=$(rewind fork $fork2 $((step + 30)) --schedule 11 --json | id)

        # Running the fork's inputs again as a plain run leaves the fork
        # with no parent, still reading a's keyframes, so a stays.
        rewind run -q --schedule 3 --schedule-from 400 --root ${busyboxRoot} -- sh -c '${workload}'
        ! grep -q '"parent": \[' "$(manifest $fork)"
        ! rewind remove a 2> refused
        cat refused
        grep -q "run $fork reads its keyframes up to step 399 from " refused
        rewind replay a --from 300 | grep '^identical'

        # Several runs go in one call, each once, and one refused among
        # them keeps them all.
        ! rewind remove $leaf1 a 2> refused
        grep -q "run $fork reads its keyframes up to step 399 from " refused
        test -d $REWIND_HOME/runs/$leaf1
        rewind remove $leaf1 $leaf2 $leaf1 --json | tee removed
        grep -q "{\"removed\":\[\"$leaf1\",\"$leaf2\"\]}" removed
        test ! -e $REWIND_HOME/runs/$leaf1
        test ! -e $REWIND_HOME/runs/$leaf2

        rewind remove $fork2 --dry-run | tee planned
        grep -q "would remove $fork2" planned
        test -d $REWIND_HOME/runs/$fork2
        rewind remove $fork --json | tee removed
        grep -q "{\"removed\":\[\"$fork\",\"$fork2\"\]}" removed
        test ! -e $REWIND_HOME/runs/$fork
        test ! -e $REWIND_HOME/runs/$fork2
        rewind replay a --from 300 | grep '^identical'

        # The removed forks' own keyframes named pages no other run's do:
        # gc removes them, a dry run first removing nothing, and the runs
        # that stay still replay from their keyframes. A second gc finds
        # nothing left.
        rewind gc --dry-run | tee planned
        grep -q '^would remove [1-9][0-9]* pages' planned
        # Into a file, not straight into grep -q: grep exits at its first
        # match, and gc's next line would then die of SIGPIPE.
        rewind gc --dry-run > planned-again
        grep -q '^would remove [1-9][0-9]* pages' planned-again
        rewind gc --json | tee collected
        grep -q '"pages":[1-9]' collected
        rewind replay a --from 300 | grep '^identical'
        rewind replay b --from 300 | grep '^identical'
        rewind gc --json | tee again
        grep -q '"pages":0' again
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
        rewind events w > events
        start=$(awk '/rewind-start/ { print $1; exit }' events)

        end=$(rewind ls --json | sed -n 's/.*"name":"w",.*"steps":\([0-9]*\).*/\1/p')

        # rewind cat: missing before the job, both lines at the end, and a
        # step past the end refused.
        status=0
        rewind cat w "$start" /notes.txt || status=$?
        test "$status" = 3
        rewind cat w "$end" /notes.txt | tee cat
        grep -q second cat
        ! rewind cat w $((end + 1)) /notes.txt 2> past
        grep -q 'past its end' past

        # rewind shell, typed into from a pipe.
        printf 'cat /notes.txt; exit\n' | rewind shell w "$end" | tee shell
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
        write=$(rewind events w | grep 'notes.txt' | sed -n 1p)
        step=$(echo "$write" | awk '{print $1}')
        pid=$(echo "$write" | awk '{print $2}' | cut -d/ -f1)
        rewind gdb w "$step" --listen 127.0.0.1:12346 2> serve &
        while ! grep -q 'connect with' serve; do sleep 0.1; done
        gdb -q -batch -ex 'target remote 127.0.0.1:12346' -ex detach
        wait
        cat serve
        grep -q "ran in process $pid; loading symbols for 1 of its files" serve

        # rewind gdb at a write the process blocks right after: the next
        # exit is the idle task's, and the process named is still the
        # writer.
        rewind run -q --name block --root ${gdbRoot} -- /bin/block
        write=$(rewind events block | grep 'write(1, "wait' | sed -n 1p)
        step=$(echo "$write" | awk '{print $1}')
        pid=$(echo "$write" | awk '{print $2}' | cut -d/ -f1)
        rewind gdb block "$step" --listen 127.0.0.1:12347 2> serve &
        while ! grep -q 'connect with' serve; do sleep 0.1; done
        gdb -q -batch -ex 'target remote 127.0.0.1:12347' -ex detach
        wait
        echo "$write"
        cat serve
        grep -q "ran in process $pid;" serve
        touch $out
      '';

  # checks.image-env: the image of a root, and so the run, is the same
  # whether rewind runs in a Nix development shell, which sets
  # SOURCE_DATE_EPOCH to 1980, or outside one. mkfs.erofs reads the
  # variable ahead of the timestamp rewind gives it. Boots the VM, so it
  # needs /dev/kvm.
  image-env =
    pkgs.runCommand "rewind-image-env"
      {
        nativeBuildInputs = [ rewind ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        REWIND_HOME=$TMPDIR/shell SOURCE_DATE_EPOCH=315532800 \
          rewind run -q --root ${busyboxRoot} -- echo hi
        env -u SOURCE_DATE_EPOCH REWIND_HOME=$TMPDIR/plain \
          rewind run -q --root ${busyboxRoot} -- echo hi
        shell=$(REWIND_HOME=$TMPDIR/shell rewind ls | awk '{print $1}')
        plain=$(REWIND_HOME=$TMPDIR/plain rewind ls | awk '{print $1}')
        echo "in a shell: $shell, outside one: $plain"
        test "$shell" = "$plain"
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
        start=$(rewind events busy | grep 'rewind-start' | sed -n 1p | awk '{print $1}')
        end=$(rewind events busy | grep 'rewind-exit' | sed -n 1p | awk '{print $1}')

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
  # stops just after each write the debugged process makes to its global,
  # with the old and new values, and the fork runs on to the end on the
  # recording. Its child writes the global at the same address in its own
  # address space first, and calls write first, which must stop gdb at
  # neither a watchpoint nor a breakpoint. x86 has no trap on reads alone,
  # so `rwatch` is refused. Boots the VM, so it needs /dev/kvm.
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
        grep -q 'New value = 2' gdb
        grep -q 'New value = 3' gdb
        ! grep -q 'New value = 1' gdb
        grep -q 'exited normally' gdb
        ! grep -q 'left the recording\|SIGTRAP' gdb

        # The child's write is passed, the parent's last one stops gdb.
        rewind gdb watch "$start" -- -batch \
          -ex 'break write' -ex continue -ex 'print counter' -ex delete -ex continue > break 2>&1 || true
        cat break
        grep -q '^\$1 = 3' break
        grep -q 'exited normally' break
        ! grep -q 'left the recording\|SIGTRAP' break

        rewind gdb watch "$start" -- -batch -ex 'rwatch counter' -ex continue > read 2>&1 || true
        cat read
        grep -q 'Could not insert' read
        touch $out
      '';

  # checks.gdb-cold: a fork made for gdb at a step with no keyframe yet
  # stays on the recording under the branch clock. Such a fork replays to
  # the step, opening its branch counters, before gdb's symbols are looked
  # up in forks of their own on the same thread; a counter counts the
  # thread's guest, whichever machine's, so a fork that counted them too
  # fired its timer early and went its own way. Two shells race under the
  # branch clock, and gdb continues to the end from every third step of
  # the run. A machine whose branch counter cannot be opened, as CI's
  # cannot, skips the check and says so. Boots the VM, so it needs
  # /dev/kvm.
  gdb-cold =
    pkgs.runCommand "rewind-gdb-cold"
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

        if ! rewind run -q --clock branches --name busy --root ${busyboxRoot} -- \
          sh -c 'for i in 1 2; do (n=0; while [ $n -lt 20000 ]; do n=$((n+1)); done; echo $i) & done; wait' \
          > run 2>&1; then
          cat run
          grep -q 'perf_event_open' run
          echo "skipped: this machine's branch counter cannot drive virtual time"
          touch $out
          exit 0
        fi
        start=$(rewind events busy | grep 'rewind-start' | sed -n 1p | awk '{print $1}')
        end=$(rewind events busy | grep 'rewind-exit' | sed -n 1p | awk '{print $1}')

        sessions=0
        for step in $(seq "$start" 3 "$end"); do
          rewind gdb busy "$step" -- -batch -ex continue > gdb 2>&1 || true
          if ! grep -q 'exited normally' gdb || grep -q 'left the recording\|SIGTRAP' gdb; then
            echo "from step $step:"
            cat gdb
            exit 1
          fi
          sessions=$((sessions + 1))
        done
        echo "$sessions forks for gdb, each made at a step without a keyframe, stayed on the recording"
        touch $out
      '';

  # checks.inspect-stops-threads: an inspection sees the machine at the step
  # it asks about, every thread stopped there. A worker prints and faults
  # at once while the main thread waits on a vfork child, a sleep no stop
  # signal wakes. At the print, the step the worker is on the CPU in, the
  # worker is stopped and the process alive, where a stop sent only to the
  # process left the worker to run on into its fault. Boots the VM, so it
  # needs /dev/kvm.
  inspect-stops-threads =
    pkgs.runCommand "rewind-inspect-stops-threads"
      {
        nativeBuildInputs = [ rewind ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        rewind run -q --clock exits --name crash --root ${gdbRoot} -- /bin/crash || true
        rewind events crash > events
        grep -q 'SIGSEGV' events
        write=$(rewind events crash | grep 'write(1, "dying')
        step=$(echo "$write" | awk '{print $1}')
        pid=$(echo "$write" | awk '{print $2}' | cut -d/ -f1)
        tid=$(echo "$write" | awk '{print $2}' | cut -d/ -f2)
        test "$pid" != "$tid"

        rewind cat crash "$step" /proc/$pid/status > status
        cat status
        ! grep -q '^State:.*zombie' status
        rewind cat crash "$step" /proc/$pid/task/$tid/stat > stat
        cat stat
        test "$(awk '{print $3}' stat)" = T
        touch $out
      '';

  # checks.gdb-threads: `rewind gdb` shows every thread of the process it
  # debugs, those off the CPU included. Three workers wait on a futex
  # while the main thread sleeps, then prints. At that print, gdb lists
  # the CPU and the four threads, and finds each worker waiting in its own
  # function, from the registers the kernel saved for it. At a step that
  # ran in no process, `--pid` names the process, and gdb still finds the
  # three. A breakpoint a worker hits stops in that worker's thread.
  # Boots the VM, so it needs /dev/kvm.
  gdb-threads =
    pkgs.runCommand "rewind-gdb-threads"
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

        rewind run -q --clock exits --name threads --root ${gdbRoot} -- /bin/threads
        write=$(rewind events threads | grep 'write(1, "open')
        open=$(echo "$write" | awk '{print $1}')
        pid=$(echo "$write" | awk '{print $2}' | cut -d/ -f1)

        # gdb at a step, given the number of threads it should list: each
        # worker waiting in its own function.
        threads_at() {
          want=$1
          shift
          rewind gdb threads "$@" -- -batch -ex 'thread apply all bt 1' > gdb 2>&1 || true
          cat gdb
          test "$(grep -c '^Thread ' gdb)" = "$want"
          test "$(grep -c '^#0 .*worker .*threads\.c' gdb)" = 3
        }
        threads_at 5 "$open"

        # The latest step before the print that ran in no process: the
        # machine idle while every thread waits.
        idle=
        for step in $(seq $((open - 1)) -1 $((open - 40))); do
          rewind gdb threads "$step" --listen 127.0.0.1:12350 2> serve &
          while ! grep -q 'connect with' serve; do sleep 0.1; done
          gdb -q -batch -ex 'target remote 127.0.0.1:12350' -ex detach > /dev/null 2>&1
          wait
          if grep -q 'in no process' serve; then
            idle=$step
            break
          fi
        done
        echo "idle at step ''${idle:-none}"
        test -n "$idle"

        threads_at 5 "$idle" --pid "$pid"

        # A breakpoint a worker hits on its way out stops that worker, the
        # thread on the CPU, not the CPU's own thread.
        rewind gdb threads "$open" -- -batch -ex 'break pthread_exit' -ex continue \
          -ex 'info threads' > hit 2>&1 || true
        cat hit
        grep -q '^Thread [2-9] hit Breakpoint 1, ' hit
        grep -q '^\* [2-9] .*on the CPU' hit
        touch $out
      '';

  # checks.gdb-int3: breakpoints past the CPU's four debug registers are
  # int3 in memory, and the fork stays on its recording through them,
  # under exit time and, where the branch counter opens, counter time. Of
  # seven breakpoints three are int3. One is in code a forked child runs
  # three times before the parent does: the child's calls are passed, and
  # the first stop is the parent's. The child's own int3 goes back to it,
  # and its handler prints. Then each of the parent's six calls stops in
  # turn, and the run ends. A breakpoint in the kernel past the registers
  # stops too, and with breakpoints left in memory, gdb reads the code's
  # own byte where int3 is. Boots the VM, so it needs /dev/kvm.
  gdb-int3 =
    pkgs.runCommand "rewind-gdb-int3"
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

        breaks=""
        for f in first second third fourth fifth sixth shared; do
          breaks="$breaks -ex 'break $f'"
        done
        for clock in exits branches; do
          if ! rewind run -q --clock "$clock" --name "int3-$clock" --root ${gdbRoot} -- /bin/int3 \
            > run 2>&1; then
            cat run
            grep -q 'perf_event_open' run
            echo "skipped counter time: this machine's branch counter cannot drive it"
            continue
          fi
          rewind events "int3-$clock" > events
          grep -q 'trapped' events
          start=$(grep 'write(1, "start' events | awk '{print $1}')

          eval "rewind gdb int3-$clock $start -- -batch $breaks" \
            -ex continue -ex continue -ex continue -ex continue -ex continue \
            -ex continue -ex continue -ex continue > gdb 2>&1 || true
          cat gdb
          grep 'hit Breakpoint' gdb > hits
          test "$(wc -l < hits)" = 7
          sed -n 1p hits > first
          grep -Eq 'Breakpoint 7, shared \(who=(who@entry=)?0\)' first
          sed -n 7p hits > last
          grep -q 'Breakpoint 6, sixth' last
          grep -q 'exited normally' gdb
          ! grep -q 'left the recording\|SIGTRAP\|no debug register' gdb
        done

        # A kernel function past the registers, and gdb's reads with
        # breakpoints left in memory.
        rewind gdb int3-exits "$start" -- -batch -ex 'break first' -ex 'break second' \
          -ex 'break third' -ex 'break fourth' -ex 'break __x64_sys_wait4' -ex continue \
          -ex 'set breakpoint always-inserted on' -ex 'break sixth' -ex 'x/1bx sixth' \
          -ex 'delete' -ex continue > kernel 2>&1 || true
        cat kernel
        grep -q 'hit Breakpoint 5, .*__x64_sys_wait4' kernel
        grep -q '^0x[0-9a-f]* <sixth>:' kernel
        ! grep -q '^0x[0-9a-f]* <sixth>:.*0xcc' kernel
        grep -q 'exited normally' kernel
        ! grep -q 'left the recording' kernel
        touch $out
      '';

  # checks.gdb-vdso: gdb knows the vDSO's code. The kernel maps the vDSO,
  # which holds clock_gettime, into every process, and it is no file, so
  # rewind reads it out of the fork's memory. From the print between the
  # program's two clock reads, a breakpoint on __vdso_clock_gettime stops
  # in the second, and the stack walks out of it into musl's
  # clock_gettime. Boots the VM, so it needs /dev/kvm.
  gdb-vdso =
    pkgs.runCommand "rewind-gdb-vdso"
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

        rewind run -q --clock exits --name vdso --root ${gdbRoot} -- /bin/vdso
        rewind events vdso > events
        again=$(grep 'write(1, "again' events | awk '{print $1}')

        rewind gdb vdso "$again" -- -batch -ex 'break __vdso_clock_gettime' -ex continue \
          -ex 'bt 2' > gdb 2>&1 || true
        cat gdb
        # gdb names the vDSO's function by its other name, clock_gettime.
        grep -q 'hit Breakpoint 1, 0x00007f[0-9a-f]* in clock_gettime ()' gdb
        grep -q '^#1  0x0000000000[0-9a-f]* in clock_gettime ()' gdb
        touch $out
      '';

  # checks.where: `rewind where` names the program's own line at a step.
  # At the worker's write, the innermost frames are musl's printf
  # machinery, which has no line table; the answer is the worker's printf
  # in pool.c, with its source from the VM and the line marked, and two
  # frames that called it. --json lists the same frame as the chosen one,
  # and carries pool.c whole with the write on that line.
  # The main thread, off the CPU then, waits in pool_run's join, called
  # from main. Boots the VM, so it needs /dev/kvm.
  where =
    pkgs.runCommand "rewind-where"
      {
        nativeBuildInputs = [
          rewind
          pkgs.gdb
          pkgs.jq
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        # No debuginfod server: without a network, each of gdb's questions
        # to it waits out a timeout.
        export REWIND_DEBUGINFOD=/nonexistent

        rewind run -q --clock exits --name pool --root ${whereRoot} -- /bin/pool
        write=$(rewind events pool | grep 'write(1, "worker')
        step=$(echo "$write" | awk '{print $1}')
        pid=$(echo "$write" | awk '{print $2}' | cut -d/ -f1)
        line() { grep -n "$1" ${whereRoot}/src/pool.c | cut -d: -f1; }

        rewind where pool "$step" > where
        cat where
        grep -q "^#[1-9][0-9]* worker (pool.c:$(line 'the write'))$" where
        grep -q "^> *$(line 'the write')  .*printf(" where
        test "$(grep -c '^called from ' where)" = 2

        # The second lookup reads the source files the first fetched from
        # the run's source cache, with no fork for them.
        rewind where pool "$step" --json > where.json 2> stderr
        cat stderr
        grep -q '^rewind: read [0-9]* source files fetched from the VM earlier$' stderr
        test "$(grep -c '^rewind: fetched ' stderr)" = 0
        jq -e --argjson line "$(line 'the write')" \
          '.frames[.chosen] | .function == "worker" and .line == $line' where.json
        jq -e --argjson line "$(line 'the write')" \
          '.frames[.chosen] as $f | .files[$f.fullname]
            | .extent == "whole" and .first == 1
              and (.text | split("\n")[$line - 1] | contains("printf("))' where.json

        rewind where pool "$step" --tid "$pid" > main
        cat main
        grep -q "^#[1-9][0-9]* pool_run (pool.c:$(line 'the wait'))$" main
        grep -q '^called from #[0-9]* main (main.c:' main

        # Removing the run takes its source cache with it.
        id=$(basename "$(dirname "$(grep -l '"name": "pool"' $REWIND_HOME/runs/*/manifest.json)")")
        test -d $REWIND_HOME/cache/sources/$id
        rewind remove pool
        test ! -e $REWIND_HOME/cache/sources/$id
        touch $out
      '';

  # checks.check-where: `rewind check --where` says where the threads
  # were when a schedule ends differently: the thread on the CPU at the
  # step that decides it, and the thread of the failing run's first event
  # that differs, each by a function and a line of race.c, in its text and
  # in its JSON. Boots the VM, so it needs /dev/kvm.
  check-where =
    pkgs.runCommand "rewind-check-where"
      {
        nativeBuildInputs = [
          rewind
          pkgs.gdb
          pkgs.jq
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        # No debuginfod server: without a network, each of gdb's questions
        # to it waits out a timeout.
        export REWIND_DEBUGINFOD=/nonexistent

        # check exits 1 when a schedule ends differently, which is what
        # this needs.
        ! rewind check --where -j 4 --clock exits --root ${raceRoot} -- /bin/race > found
        cat found
        grep -q ' decides it: ' found
        # A reschedule decides it only where a thread is between its read
        # and its store, in the write: add, at that line, not the C
        # library's inline write it calls.
        exit_line=$(grep -n 'the exit' ${raceRoot}/src/race.c | cut -d: -f1)
        thread='^ +[0-9]+ +[0-9]+/[0-9]+ +'
        grep -qE "$thread"'add \(race\.c:'"$exit_line"'\), on the CPU at the deciding step$' found
        grep -qE "$thread"'[a-z_]+ \(race\.c:[0-9]+\), at the first event that differs$' found

        ! rewind check --where --json -j 4 --clock exits --root ${raceRoot} -- /bin/race > found.json
        jq -e '.narrowed.threads | length == 2 and all(.frame.file == "race.c")' found.json
        touch $out
      '';

  # checks.check-run: `rewind check --run` tries schedules on a run already
  # recorded, each a fork of it at --schedule-from, instead of recording
  # the job again. With --all and --no-narrow it counts the forks that end
  # differently and stops; without them it narrows the first such fork to
  # the step that decides it, as for a job. Boots the VM, so it needs
  # /dev/kvm.
  check-run =
    pkgs.runCommand "rewind-check-run"
      {
        nativeBuildInputs = [
          rewind
          pkgs.jq
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        rewind run -q --clock exits --name race --root ${raceRoot} -- /bin/race
        id=$(rewind ls --json -n 1 | jq -r .id)
        # The step the second worker starts on: both threads exist after it.
        from=$(rewind events race | grep 'clone(CLONE_THREAD)' | tail -1 | awk '{print $1}')

        # The sweep: eight forks of the run at the step, counted. check
        # exits 1 when any of them ends differently, and says how many.
        rewind check --run race --schedule-from "$from" --schedules 8 --all \
          --no-narrow --json > swept.json || true
        jq -e '.tried == 8 and (.schedules | length) == 9 and .narrowed == null' swept.json
        jq -e --arg id "$id" '.schedules[0].id == $id' swept.json
        jq -e --arg id "$id" --argjson from "$from" \
          '.schedules[1:] | all(.parent.run == $id and .parent.step == $from)' swept.json
        jq -e '.differing == ([.schedules[1:][] | select(.differs)] | length)' swept.json
        jq -e '.schedules[0].differs == false' swept.json
        test "$(rewind ls --forks-of "$id" | wc -l)" = 8

        # The search: the first fork that ends differently, narrowed.
        ! rewind check --run race --schedule-from "$from" > found
        cat found
        grep -q ' decides it: ' found

        # Options that make another run than the one named are refused.
        ! rewind check --run race --mem 2048 2> refused
        grep -q -- '--mem' refused
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
