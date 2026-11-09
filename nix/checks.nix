# `nix flake check`: everything CI asserts, offline.
{
  pkgs,
  rewind,
  kernel,
  guest,
  gdb,
  module,
  rewindLib,
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
        programs.rewind.systemFeature = "amd";
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

        cat > late.c <<'EOF'
        #include <unistd.h>

        volatile int counter;

        int main(void)
        {
          counter = 7;
          for (int i = 0; i < 30000; i++)
            write(1, ".", 1);
          write(1, "done\n", 5);
          return 0;
        }
        EOF
        $CC -static -O1 -g -o $out/bin/late late.c

        cat > fields.c <<'EOF'
        #include <stddef.h>
        #include <stdint.h>
        #include <unistd.h>

        union pair {
          uint64_t whole;
          struct { uint32_t low, high; } fields;
        };

        volatile union pair pair;
        volatile uint64_t counter;
        unsigned char buffer[6] = { 1, 2, 3, 4, 5, 6 };

        int main(void)
        {
          unsigned char *source = buffer + 1, *destination = buffer;
          size_t count = 5;

          pair.whole = 0x0000000200000001;
          pair.fields.high = 5;
          for (uint64_t i = 0; i < 20000; i++)
            counter = i;
          __asm__ volatile("rep movsb" : "+S"(source), "+D"(destination), "+c"(count) : : "memory");
          write(1, "done\n", 5);
          return 0;
        }
        EOF
        $CC -static -O1 -g -o $out/bin/fields fields.c

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

        # The source is in the root under /src, the directory its DWARF
        # names, so gdb shows its lines.
        mkdir -p $out/src
        cat > $out/src/reverse.c <<'EOF'
        #include <unistd.h>

        volatile long total;

        __attribute__((noinline)) void add(long n)
        {
          total += n; /* the add */
        }

        int main(void)
        {
          write(1, "start\n", 6);
          for (long i = 1; i <= 5; i++) {
            add(i); /* the call */
            write(1, ".", 1); /* the dot */
          }
          write(1, "done\n", 5);
          return 0;
        }
        EOF
        $CC -static -O0 -g -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/reverse $out/src/reverse.c

        cat > $out/src/burst.c <<'EOF'
        #include <unistd.h>

        volatile long total;

        int main(void)
        {
          write(1, "start\n", 6);
          for (long i = 1; i <= 5; i++)
            total += i; /* the add */
          write(1, "done\n", 5);
          return 0;
        }
        EOF
        $CC -static -O0 -g -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/burst $out/src/burst.c

        cat > $out/src/slow.c <<'EOF'
        #include <unistd.h>

        volatile unsigned long sum;

        int main(void)
        {
          write(1, "begin\n", 6);
          for (unsigned long i = 0; i < 10000000; i++)
            sum += i;
          write(1, "done\n", 5);
          return 0;
        }
        EOF
        $CC -static -O1 -g -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/slow $out/src/slow.c

        cat > $out/src/piped.c <<'EOF'
        #include <sys/wait.h>
        #include <time.h>
        #include <unistd.h>

        char buffer[8];

        int main(void)
        {
          int fds[2];
          pipe(fds);
          write(1, "start\n", 6);
          if (fork() == 0) {
            struct timespec pause = { 0, 20000000 };
            nanosleep(&pause, 0);
            write(fds[1], "hello", 5);
            _exit(0);
          }
          read(fds[0], buffer, sizeof buffer); /* the read */
          write(1, "done\n", 5);
          wait(0);
          return 0;
        }
        EOF
        $CC -static -O0 -g -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/piped $out/src/piped.c

        # The main thread waits in recv() for 16 bytes of a socket while a
        # second thread sends 5, sleeps, writes a global, and signals it.
        # The kernel copies the 5 bytes into the buffer, waits on, and
        # returns them once the signal comes, into the handler. The first
        # argument picks what the handler does: write() and return (r),
        # _exit (e), or siglongjmp back into main (j); or with k, there is
        # no handler, and the signal kills the process in the recv.
        cat > $out/src/handled.c <<'EOF'
        #include <pthread.h>
        #include <setjmp.h>
        #include <signal.h>
        #include <sys/socket.h>
        #include <time.h>
        #include <unistd.h>

        char buffer[16];
        int fds[2];
        pthread_t reader;
        char mode;
        int other;
        sigjmp_buf back;

        void handler(int sig)
        {
          if (mode == 'e')
            _exit(0);
          if (mode == 'j')
            siglongjmp(back, 1);
          write(1, "handler\n", 8); /* the handler's write */
        }

        void signal_reader(void)
        {
          pthread_kill(reader, SIGUSR1);
        }

        void *sender(void *unused)
        {
          struct timespec pause = { 0, 20000000 };
          nanosleep(&pause, 0);
          send(fds[1], "hello", 5, 0);
          nanosleep(&pause, 0);
          other = 1;
          signal_reader();
          return 0;
        }

        int main(int argc, char **argv)
        {
          struct sigaction action = { .sa_handler = handler };
          pthread_t thread;
          mode = argv[1][0];
          if (mode != 'k')
            sigaction(SIGUSR1, &action, 0);
          socketpair(AF_UNIX, SOCK_STREAM, 0, fds);
          reader = pthread_self();
          write(1, "start\n", 6);
          pthread_create(&thread, 0, sender, 0);
          if (sigsetjmp(back, 1) == 0)
            recv(fds[0], buffer, sizeof buffer, MSG_WAITALL); /* the recv */
          write(1, "done\n", 5);
          pthread_join(thread, 0);
          return 0;
        }
        EOF
        $CC -static -O0 -g -pthread -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/handled $out/src/handled.c

        # One read() of a 1 MiB file, of x's, into a buffer as large.
        head -c 1048576 /dev/zero | tr '\0' x > $out/large
        cat > $out/src/large.c <<'EOF'
        #include <fcntl.h>
        #include <unistd.h>

        char buffer[1 << 20];

        int main(void)
        {
          int fd = open("/large", O_RDONLY);
          write(1, "start\n", 6);
          read(fd, buffer, sizeof buffer); /* the read */
          write(1, "done\n", 5);
          return 0;
        }
        EOF
        $CC -static -O0 -g -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/large $out/src/large.c

        # One program adds to a global in a loop, then execs the other,
        # which calls a function three times.
        cat > $out/src/before.c <<'EOF'
        #include <unistd.h>

        volatile long work;

        int main(void)
        {
          write(1, "before\n", 7);
          for (long i = 0; i < 100; i++)
            work += i; /* the work */
          write(1, "exec\n", 5);
          execl("/bin/after", "after", (char *)0);
          return 1;
        }
        EOF
        $CC -static -O0 -g -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/before $out/src/before.c

        cat > $out/src/after.c <<'EOF'
        #include <unistd.h>

        volatile long counter;

        __attribute__((noinline)) void bump(long n)
        {
          counter += n; /* the bump */
        }

        int main(void)
        {
          write(1, "after\n", 6);
          for (long i = 1; i <= 3; i++)
            bump(i);
          write(1, "done\n", 5);
          return 0;
        }
        EOF
        $CC -static -O0 -g -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/after $out/src/after.c

        # One program writes a line and exits: the step before each of
        # its two records runs the system call's syscall instruction.
        cat > $out/src/last.c <<'EOF'
        #include <unistd.h>

        int main(void)
        {
          write(1, "last\n", 5);
          return 0;
        }
        EOF
        $CC -static -O0 -g -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/last $out/src/last.c

        # One program writes a line, spins for a second or two in a loop
        # with no exit, then execs the program that calls bump.
        cat > $out/src/spin.c <<'EOF'
        #include <unistd.h>

        volatile unsigned long spun;

        int main(void)
        {
          write(1, "spin\n", 5);
          for (unsigned long i = 0; i < 6000000000UL; i++)
            spun = i;
          execl("/bin/after", "after", (char *)0);
          return 1;
        }
        EOF
        $CC -static -O1 -g -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/spin $out/src/spin.c

        # One program runs ud2, an invalid opcode, whose SIGILL handler runs
        # hlt, which user space may not, and whose SIGSEGV handler exits.
        cat > $out/src/ill.c <<'EOF'
        #include <signal.h>
        #include <unistd.h>

        static void on_segv(int sig)
        {
          (void)sig;
          write(1, "segv\n", 5);
          _exit(0);
        }

        static void on_ill(int sig)
        {
          (void)sig;
          write(1, "ill\n", 4);
          __asm__ volatile("hlt");
        }

        int main(void)
        {
          signal(SIGILL, on_ill);
          signal(SIGSEGV, on_segv);
          write(1, "start\n", 6);
          __asm__ volatile("ud2");
          return 1;
        }
        EOF
        $CC -static -O1 -g -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/ill $out/src/ill.c
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

  # A root for checks.where-idle: a program built from 200 source files
  # under a long directory, whose 12 workers each open a file 600 times,
  # each open and close a step, then sleep while main joins them, so the
  # CPU sits idle with every thread alive. The sources are at /src in the
  # VM only, so `rewind where` asks the VM for all 200 paths at once.
  idleRoot =
    pkgs.runCommand "rewind-idle-root"
      {
        nativeBuildInputs = [ pkgs.pkgsStatic.stdenv.cc ];
        dontStrip = true;
      }
      ''
        mkdir -p $out/bin $out/src/a-source-tree-with-a-long-name/units
        cd $out/src
        units=a-source-tree-with-a-long-name/units
        for i in $(seq 1 200); do
          echo "int unit_$i(int x) { return x + $i; }" > $units/unit_with_a_long_name_$i.c
        done

        cat > main.c <<'EOF'
        #include <fcntl.h>
        #include <pthread.h>
        #include <unistd.h>

        int unit_1(int x);

        static void *worker(void *arg)
        {
          for (int i = 0; i < 600; i++) {
            close(open("/tmp/opened", O_CREAT | O_RDONLY, 0600));
          }
          usleep(20000); /* the sleep */
          return arg;
        }

        int main(void)
        {
          pthread_t t[12];

          for (int i = 0; i < 12; i++) {
            pthread_create(&t[i], 0, worker, 0);
          }
          for (int i = 0; i < 12; i++) {
            pthread_join(t[i], 0); /* the wait */
          }
          return unit_1(0) == 1 ? 0 : 1;
        }
        EOF
        $CC -static -O1 -g -pthread -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/idle main.c $units/*.c
      '';

  # A root with a dynamic program and two shared libraries of the same
  # shape, for checks.blame-memory: actual_writer in first.so writes the
  # program's total, the program unloads first.so and loads second.so,
  # which the loader maps where first.so was, never_called at the address
  # actual_writer had. The program says whether the two functions shared
  # an address, and writes a line after the write and after the reload.
  # The programs and their sources are in the store, copied into the
  # root's /nix/store with the C library, where the VM finds them at the
  # same paths as this machine.
  reloadRoot =
    let
      programs = pkgs.runCommandCC "rewind-reload-programs" { dontStrip = true; } ''
        mkdir -p $out/bin $out/lib $out/src
        cd $out/src
        cat > first.c <<'EOF'
        void actual_writer(int *value)
        {
          *value = 7;
        }
        EOF

        cat > second.c <<'EOF'
        void never_called(int *value)
        {
          *value = 9;
        }
        EOF

        cat > reload.c <<'EOF'
        #include <dlfcn.h>
        #include <unistd.h>

        typedef void (*writer)(int *);

        int total;

        int main(void)
        {
          void *first = dlopen(FIRST, RTLD_NOW);
          writer actual = (writer)dlsym(first, "actual_writer");
          actual(&total);
          write(1, "written\n", 8);
          dlclose(first);

          void *second = dlopen(SECOND, RTLD_NOW);
          writer other = (writer)dlsym(second, "never_called");
          if (other == actual)
            write(1, "same address\n", 13);
          write(1, "reloaded\n", 9);
          return 0;
        }
        EOF
        $CC -shared -fPIC -O1 -g -o $out/lib/first.so first.c
        $CC -shared -fPIC -O1 -g -o $out/lib/second.so second.c
        $CC -O1 -g -DFIRST="\"$out/lib/first.so\"" -DSECOND="\"$out/lib/second.so\"" \
          -o $out/bin/reload reload.c
      '';
      closure = pkgs.closureInfo { rootPaths = [ programs ]; };
    in
    pkgs.runCommand "rewind-reload-root" { } ''
      mkdir -p $out/bin $out/nix/store
      while read -r path; do
        cp -a "$path" $out/nix/store/
      done < ${closure}/store-paths
      ln -s ${programs}/bin/reload $out/bin/reload
    '';

  # A root with a static program whose two threads add to a total without
  # a lock: each reads it, writes a dot, which is an exit a schedule can
  # reschedule at, and stores what it read plus one. A reschedule between
  # the read and the store loses an add, and the program exits 1. main
  # writes the total itself, with dprintf, so the write is main's at a
  # line of race.c; printf's would wait in stdio's buffer for exit to
  # write it, after main had returned. The source is in the root under
  # /src, the directory its DWARF names.
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
          dprintf(1, "\ntotal %ld\n", total);
          return total != WORKERS * ADDS;
        }
        EOF
        $CC -static -O1 -g -pthread -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/race race.c
      '';

  # A root with a static program whose two threads add to a total with an
  # atomic load and a separate atomic store, making no system call between
  # them. Without points the guest never switches threads there, so no
  # schedule loses an add. With `points` as its argument, each add puts the
  # SDK's rewind_point between the load and the store, and a schedule that
  # switches threads at one loses an add, and the program exits 1.
  pointRoot =
    pkgs.runCommand "rewind-point-root"
      {
        nativeBuildInputs = [ pkgs.pkgsStatic.stdenv.cc ];
        dontStrip = true;
      }
      ''
        mkdir -p $out/bin $out/src
        cd $out/src
        cp ${../sdk/c/rewind.h} rewind.h
        cat > point.c <<'EOF'
        #include <pthread.h>
        #include <stdio.h>
        #include <string.h>

        #include "rewind.h"

        #define WORKERS 2
        #define ADDS 10

        static long total;
        static int points;

        static void *add(void *arg)
        {
          for (int i = 0; i < ADDS; i++) {
            long seen = __atomic_load_n(&total, __ATOMIC_RELAXED);
            if (points)
              rewind_point();
            __atomic_store_n(&total, seen + 1, __ATOMIC_RELAXED);
          }
          return 0;
        }

        int main(int argc, char **argv)
        {
          pthread_t t[WORKERS];

          points = argc > 1 && strcmp(argv[1], "points") == 0;
          for (int i = 0; i < WORKERS; i++)
            pthread_create(&t[i], 0, add, 0);
          for (int i = 0; i < WORKERS; i++)
            pthread_join(t[i], 0);
          dprintf(1, "total %ld\n", total);
          return total != WORKERS * ADDS;
        }
        EOF
        $CC -static -O1 -g -pthread -fdebug-prefix-map=$out/src=/src \
          -o $out/bin/point point.c
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

  # Shell for a check that serves a fork to gdb in the background, as
  # `rewind gdb RUN STEP --listen ADDRESS 2> serve &`: `listening PID`
  # returns once the server with that PID says where to connect, and
  # fails the check, printing what the server said, when the server
  # exits first or says nothing within a minute.
  listening = ''
    listening() {
      local server=$1
      local limit_seconds=60
      local deadline=$((SECONDS + limit_seconds))
      while [ "$SECONDS" -lt "$deadline" ]; do
        if grep -qs 'connect with' serve; then
          return 0
        fi
        if ! kill -0 "$server" 2> /dev/null; then
          break
        fi
        sleep 0.1
      done
      echo "rewind gdb --listen did not start listening:"
      cat serve
      return 1
    }
  '';
  # Probes for sources of difference past the monitor, for
  # checks.determinism-sources.
  determinismRoot = import ./determinism { inherit pkgs; };
  # Programs in five languages for checks.gdb-languages.
  languagesRoot = import ./gdb-languages { inherit pkgs; };
  # The Rewind SDK's libraries in use, for checks.sdk-languages.
  sdkLanguages = import ./sdk-languages { inherit pkgs; };
in
{
  home-on-nfs = import ./home-on-nfs.nix {
    inherit pkgs rewind;
  };

  # The examples that copy an SDK library, as a program would, hold the
  # library as sdk/ has it.
  sdk-copies = pkgs.runCommand "rewind-sdk-copies" { } ''
    cmp ${../sdk/c/rewind.h} ${../examples/bank/rewind.h}
    cmp ${../sdk/c/rewind.h} ${../examples/counter/rewind.h}
    cmp ${../sdk/java/dev/rewindvm/sdk/Rewind.java} ${../examples/bank-java/dev/rewindvm/sdk/Rewind.java}
    cmp ${../sdk/java/dev/rewindvm/sdk/RewindExtension.java} ${../examples/bank-java/dev/rewindvm/sdk/RewindExtension.java}
    touch $out
  '';

  # The guest's init passes the engine's lints (nix/guest.nix).
  init-clippy = guest.clippy;

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

        # A run stopped at its time limit replays to the step it stopped
        # at and no further: the guest went on only in the replay, and
        # its events after that step were never recorded.
        rewind run -q --name late --timeout 1 --root ${busyboxRoot} -- \
          sh -c 'for i in $(seq 1000000); do echo $i; done' > /dev/null || true
        late=$(dirname "$(grep -l '"name": "late"' $REWIND_HOME/runs/*/manifest.json)")
        grep -q '"how": "timed_out"' $late/manifest.json
        rewind replay late | tee replay-late
        grep -q '^identical' replay-late

        # A look inside a run more than 512 steps past its nearest
        # keyframe keeps one at its step. A second look there keeps none,
        # since it restores that keyframe and replays nothing, and the run
        # replays from the kept keyframe like from any other. Keyframes
        # are spaced by the guest's work, 10,000 to 13,000 steps of
        # writes apart, so a run of 3,000 writes has one only at step 256.
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
        rewind replay long --from "$far" | grep "^identical to the recording from the snapshot at step $far "

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
        grep -q '"shared_keyframes": {' "$(manifest $fork)"
        grep -q '"trace_hash": "' "$(manifest $fork)"
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
        rewind replay $fork2 --from $((step - 1)) | grep "^identical to the recording from the snapshot at step $((step - 1)) "
        rewind replay $fork2 --from "$(end $fork2)" | grep '^identical'

        # A replayable export of the fork of a fork carries every keyframe
        # it reads, and replays where neither parent is.
        rewind export --replayable -o fork2.rwd $fork2
        REWIND_HOME=$TMPDIR/elsewhere rewind import fork2.rwd
        REWIND_HOME=$TMPDIR/elsewhere rewind replay $fork2 --from 300 | grep '^identical'

        # Without its parent, a fork says which run it needs.
        mv $REWIND_HOME/runs/$fork $TMPDIR/away
        if rewind replay $fork2 --from 300 2> missing; then exit 1; fi
        grep "$fork" missing
        mv $TMPDIR/away $REWIND_HOME/runs/$fork

        # A fork past the end of the run is refused. Forks at its end are
        # the run itself: prune removes them and keeps the run and the fork
        # that ran differently.
        if rewind fork a $(($(end a) + 1)) --schedule 7 2> past; then exit 1; fi
        grep -q 'past its end' past
        same1=$(rewind fork a "$(end a)" --schedule 7 --json | id)
        same2=$(rewind fork a "$(end a)" --schedule 8 --json | id)
        grep -q '"first_difference": null' "$(manifest $same1)"
        grep -q '"first_difference": [0-9]' "$(manifest $fork)"
        rewind prune a --identical --dry-run --json | tee planned
        grep -q "$same1" planned
        grep -q "$same2" planned
        test "$(grep -c "$fork" planned)" = 0
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
        grep -q '"parent": null' "$(manifest $fork)"
        if rewind remove a 2> refused; then exit 1; fi
        cat refused
        grep -q "run $fork reads its keyframes up to step 399 from " refused
        rewind replay a --from 300 | grep '^identical'

        # Several runs go in one call, each once, and one refused among
        # them keeps them all.
        if rewind remove $leaf1 a 2> refused; then exit 1; fi
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

        # A look far into a run recorded without keyframes, as rewind
        # check records most of its runs, keeps a keyframe on the way
        # after each ten seconds of the guest's work, as well as one at
        # its step. The run replays exactly from one kept on the way, and
        # the kept keyframes are the run's like any other: removing the
        # run and collecting takes their pages with it. Two million writes
        # are about two million steps, some 44 s of work at 22 us a step.
        bare_writes='for i in $(seq 2000000); do echo $i; done'
        rewind run -q --no-keyframes --name bare --root ${busyboxRoot} -- sh -c "$bare_writes"
        bare=$(dirname "$(grep -l '"name": "bare"' $REWIND_HOME/runs/*/manifest.json)")
        test ! -e "$bare/keyframes"
        end=$(rewind events bare | grep 'mark "rewind-exit ' | awk '{print $1}')
        rewind cat bare "$end" /bin/busybox > /dev/null
        bare_keyframes() { ls "$bare/keyframes" | grep '\.kf$' | sed 's/\.kf$//; s/^0*//'; }
        echo "a look at step $end of bare kept keyframes at: $(bare_keyframes | tr '\n' ' ')"
        test "$(bare_keyframes | wc -l)" -ge 2
        test "$(bare_keyframes | tail -1)" = "$end"
        along=$(bare_keyframes | sed -n 1p)
        rewind replay bare --from "$along" | grep "^identical to the recording from the snapshot at step $along "

        # Asked with --progress json, or with REWIND_PROGRESS=json as the
        # desktop app asks, a look inside a run says how far its replay
        # got on standard error, a line of JSON as it starts and one at
        # its step. Without a terminal and without asking, it says
        # nothing.
        mid=$((end / 2))
        rewind --progress json cat bare "$mid" /bin/busybox 2> said > /dev/null
        cat said
        grep -q "^{\"replaying\":{\"run\":\"[0-9a-f]*\",\"from\":[0-9]*,\"at\":$mid,\"to\":$mid," said
        test "$(grep -c '"replaying"' said)" -ge 2
        REWIND_PROGRESS=json rewind cat bare $((mid + 1000)) /bin/busybox 2> said-env > /dev/null
        grep -q '"replaying"' said-env
        rewind cat bare $((mid + 2000)) /bin/busybox 2> quiet > /dev/null
        test ! -s quiet
        rewind remove bare --json
        rewind gc --json | tee collected-bare
        grep -q '"pages":[1-9]' collected-bare
        touch $out
      '';

  # checks.state-replay: a replay is the recording's whole machine, not
  # only its events. From boot, the machine matches at every keyframe;
  # from each keyframe, at every later one, which shows restoring leaves
  # nothing out; and comparing only at the last keyframe, with no capture
  # before it, it matches there too, which shows taking keyframes changes
  # nothing the guest sees. A fork matches at the keyframes it shares
  # with its parent and its own. A keyframe whose last page is made to
  # name other contents must make the replay differ, or the comparison
  # could be comparing nothing. Needs /dev/kvm.
  state-replay =
    pkgs.runCommand "rewind-state-replay"
      {
        nativeBuildInputs = [ rewind ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        # Keyframes are spaced by the guest's work, 10,000 to 13,000
        # steps of writes apart after the first at step 256, so 40,000
        # writes take four.
        writes='for i in $(seq 40000); do echo $i; done; head -c 64 /dev/urandom | od -An -tx1'
        rewind run -q --name a --root ${busyboxRoot} -- sh -c "$writes"
        a=$(dirname "$(grep -l '"name": "a"' $REWIND_HOME/runs/*/manifest.json)")
        keyframes() { ls "$a/keyframes" | grep '\.kf$' | sed 's/\.kf$//; s/^0*//'; }
        echo "keyframes: $(keyframes | tr '\n' ' ')"
        test "$(keyframes | wc -l)" -ge 3

        # Recording the run again, in a home of its own where it keeps no
        # keyframes from the first, takes its keyframes at the same steps:
        # they follow the guest's progress, not the host's time for it.
        REWIND_HOME=$TMPDIR/again rewind run -q --name a --root ${busyboxRoot} -- sh -c "$writes" > /dev/null
        again=$TMPDIR/again/runs/$(basename "$a")
        ls "$again/keyframes" | grep '\.kf$' | sed 's/\.kf$//; s/^0*//' > again-keyframes
        echo "keyframes again: $(tr '\n' ' ' < again-keyframes)"
        keyframes | diff - again-keyframes

        # A job too short for a second keyframe by its work still gets one
        # where init reports its exit, so a replay compares the whole
        # machine after the job ran as well as after boot.
        rewind run -q --name short --root ${busyboxRoot} -- sh -c true
        short=$(dirname "$(grep -l '"name": "short"' $REWIND_HOME/runs/*/manifest.json)")
        exited=$(rewind events short --json | sed -n 's/^{"step":\([0-9]*\),.*"text":"rewind-exit .*/\1/p')
        ls "$short/keyframes" | grep '\.kf$' | sed 's/\.kf$//; s/^0*//' > short-keyframes
        echo "short keyframes: $(tr '\n' ' ' < short-keyframes), job exit at step $exited"
        test "$(tail -1 short-keyframes)" = "$exited"
        rewind replay short | tee short-replay
        grep -q '^identical to the recording: .*, and the whole machine at its 2 keyframes$' short-replay

        rewind replay a | tee from-boot
        grep -q "^identical to the recording: .*, and the whole machine at its $(keyframes | wc -l) keyframes$" from-boot

        for k in $(keyframes | sed '$d'); do
          rewind replay a --from "$k" | tee from-$k
          grep -q "^identical to the recording from the snapshot at step $k to the end, and the whole machine" from-$k
        done

        rewind replay a --machine last | tee last
        grep -q '^identical to the recording: .*, and the whole machine at its keyframe$' last

        fork=$(rewind fork a 400 --schedule 3 --json | sed -n 's/.*"id":"\([0-9a-f]*\)".*/\1/p')
        rewind replay $fork | tee fork
        grep -q '^identical to the recording: .*, and the whole machine at its' fork

        # The negative test: the last keyframe names other contents for
        # its last page, the last 32 bytes of its file. Its events stay
        # the same, the machine does not, and a look before it warns.
        last=$(ls "$a/keyframes" | grep '\.kf$' | tail -1)
        size=$(stat -c %s "$a/keyframes/$last")
        head -c 32 /dev/zero | tr '\0' '\377' |
          dd of="$a/keyframes/$last" bs=1 seek=$((size - 32)) conv=notrunc 2> /dev/null
        if rewind replay a --machine last > tampered; then exit 1; fi
        cat tampered
        grep -q '^identical to the recording: ' tampered
        grep -q '^the machine is DIFFERENT from the recording at the keyframe at step ' tampered
        grep -q 'not in the page store' tampered
        grep -q 'not deterministic on this host' tampered
        grep -q 'rewind replay [0-9a-f]* --bisect' tampered

        # The same difference, with the manifest naming another build as
        # the run's recorder: replay names both builds and sends the
        # reader to the one that recorded the run, not to --bisect, whose
        # two replays would both be by this build.
        sed -i 's/"recorded_by": "[^"]*"/"recorded_by": "0.8.0 (a211b3d6fefe)"/' "$a/manifest.json"
        if rewind replay a --machine last > other-build; then exit 1; fi
        cat other-build
        grep -q '^the machine is DIFFERENT from the recording at the keyframe at step ' other-build
        grep -q '^rewind 0.8.0 (a211b3d6fefe) recorded the run, and this is rewind ' other-build
        grep -q 'Replay the run with rewind 0.8.0 (a211b3d6fefe)' other-build
        test "$(grep -c -e '--bisect' -e 'not deterministic' other-build)" = 0
        if rewind replay a --machine last --json > other-build.json; then exit 1; fi
        grep -q '"builds":"other"' other-build.json
        grep -q '"recorded_by":"0.8.0 (a211b3d6fefe)"' other-build.json
        touch $out
      '';

  # checks.determinism-sources: each probe in nix/determinism reaches for
  # something a guest could read from the host past the monitor, and is
  # recorded and replayed. The hardware counters perf_event_open offers
  # must be absent, since KVM would count them on the host. rdrand is the
  # one known gap: on AMD the CPU runs it whatever CPUID says, and the
  # replay of a run that prints a number it drew differs; on Intel KVM
  # faults it. Needs /dev/kvm.
  determinism-sources =
    pkgs.runCommand "rewind-determinism-sources"
      {
        nativeBuildInputs = [ rewind ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind

        # Every probe but rdrand's replays exactly, run twice under two
        # seeds so a value that leaked would differ between them as well.
        for probe in counters xinuse results; do
          rewind run --name $probe --root ${determinismRoot} -- /bin/$probe | tee $probe.out
          rewind replay $probe | tee $probe.replay
          grep -q '^identical' $probe.replay
          rewind run --seed 1 --name $probe-1 --root ${determinismRoot} -- /bin/$probe > $probe-1.out
          cmp $probe.out $probe-1.out
        done
        rewind run --name cpuinfo --root ${determinismRoot} -- cat /proc/cpuinfo > cpuinfo
        rewind replay cpuinfo | grep '^identical'

        # The guest has no hardware counters to read.
        grep -qx 'cycles: none, .*' counters.out
        grep -qx 'instructions: none, .*' counters.out

        # On Intel, a restore marks in use each part its header names, and
        # the guest kernel's headers name x87 and SSE, so XINUSE reads 0x3
        # at the start and after a vzeroall and a fork on every kind of
        # core. AMD's CPUs report the registers' values instead.
        if grep -q GenuineIntel /proc/cpuinfo; then
          grep -qx 'xinuse at start: 0x3' xinuse.out
          grep -qx 'xinuse after vzeroall and a fork: 0x3' xinuse.out
        fi

        if grep -q AuthenticAMD /proc/cpuinfo; then
          rewind run -q --name rdrand --root ${determinismRoot} -- /bin/rdrand
          if rewind replay rdrand > rdrand.replay; then exit 1; fi
          cat rdrand.replay
          grep -q '^DIFFERENT' rdrand.replay

          # hidden keeps the number it drew and prints a fixed word: its
          # events replay identically, its machine does not, and the
          # replay names the variable that holds the number. A look
          # before that keyframe warns that it may show values the
          # recording never had. The clock is exit time, which the report
          # below names, on a machine whose sandbox shows the branch
          # counter's workaround as on one whose sandbox does not.
          rewind run -q --clock exits --name hidden --root ${determinismRoot} -- /bin/hidden
          if rewind replay hidden > hidden.replay; then exit 1; fi
          cat hidden.replay
          grep -q '^identical to the recording: ' hidden.replay
          grep -q '^the machine is DIFFERENT from the recording at the keyframe at step ' hidden.replay
          grep -q 'secret, a variable (hidden.c:' hidden.replay
          differs=$(sed -n 's/^the machine is DIFFERENT from the recording at the keyframe at step \([0-9]*\)\..*/\1/p' hidden.replay)
          rewind cat hidden $((differs - 1)) /bin/hidden 2> look.err > /dev/null
          grep -q 'can be values the recording never had' look.err

          # --bisect narrows it to the step and the instruction: rdrand in
          # hidden's main, with why and what the program can do instead.
          if rewind replay hidden --bisect > hidden.bisect; then exit 1; fi
          cat hidden.bisect
          grep -q '^the machine first differs at step ' hidden.bisect
          grep -q 'process [0-9]* (hidden) ran  rdrand r' hidden.bisect
          grep -q 'in main' hidden.bisect
          grep -q 'getrandom()' hidden.bisect

          # rewind bugreport gathers the replay and the host into
          # report.md, with the home directories left out of its paths,
          # writes the run's trace-only export, small enough for an
          # issue, and prints a link that opens an issue filled in with
          # the report. With --no-run it writes no export.
          rewind bugreport hidden -o report > report.out
          cat report.out
          grep -q '^open an issue with the report filled in: https://github.com/fzakaria/rewindvm/issues/new?title=Run%20' report.out
          grep -q 'the machine is DIFFERENT' report/report.md
          grep -q '^- vendor_id: AuthenticAMD' report/report.md
          grep -q '^- clock: exit time$' report/report.md
          test "$(grep -c "$REWIND_HOME" report/report.md)" = 0
          test "$(stat -c %s report/*.rwd)" -lt 1000000
          rewind bugreport hidden --no-run -o bare > /dev/null
          test -z "$(find bare -name '*.rwd')"

          # Executing the run's inputs again keeps its keyframes, makes
          # another trace, and blames the host rather than the build.
          if rewind run --name rdrand --root ${determinismRoot} -- /bin/rdrand 2> again.err; then exit 1; fi
          cat again.err
          grep -q 'not deterministic on this host' again.err

          # rewind check sees schedules of draw, whose exit status rdrand
          # picks, end differently, and before it blames one it replays
          # both sides, which do not replay exactly: the check stops there
          # and blames the host.
          if rewind check --schedules 16 --root ${determinismRoot} -- /bin/draw > rdrand.check 2>&1; then exit 1; fi
          cat rdrand.check
          grep -q 'no schedule decides this' rdrand.check
          grep -q 'not deterministic on this host' rdrand.check
          test "$(grep -c 'step .* decides it' rdrand.check)" = 0
        else
          if rewind run --name rdrand --root ${determinismRoot} -- /bin/rdrand 2> rdrand.err; then exit 1; fi
          cat rdrand.err
          grep -q 'killed:SIGILL' rdrand.err
        fi
        touch $out
      '';

  # checks.cores: with --cores 4, the affinity system call busybox's nproc
  # reads, the sysfs list glibc reads and /proc/cpuinfo all say 4. Pinning to one of the four succeeds and to
  # a fifth fails, as on a machine with four. A pinned task reads back the
  # CPUs it was pinned to, and so does a child it starts, as libuv's
  # thread_affinity test expects. With the default, all say 1.
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
          echo "nproc pinned $(taskset -c 2 nproc)"
          taskset -c 1,2 sh -c "taskset -p \$\$" | grep -o "mask: [0-9a-f]*" || echo "refused 1,2"
        '
        rewind run -q --name four --cores 4 --root ${busyboxRoot} -- sh -c "$probe"
        rewind log four | tee four
        grep -qx 'nproc 4' four
        grep -qx 'online 0-3' four
        grep -qx 'cpuinfo 4' four
        grep -qx 'pinned 3' four
        grep -qx 'refused 4' four
        grep -qx 'nproc pinned 1' four
        grep -qx 'mask: 6' four

        rewind run -q --name one --root ${busyboxRoot} -- sh -c "$probe"
        rewind log one | tee one
        grep -qx 'nproc 1' one
        grep -qx 'online 0' one
        grep -qx 'cpuinfo 1' one
        grep -qx 'refused 3' one
        grep -qx 'refused 1,2' one

        rewind replay four | grep '^identical'
        touch $out
      '';

  # checks.inspect: looking inside a recorded run at a step. A file read
  # before and after it is written, a shell that reads it, a directory
  # listed and archived, and gdb stopping at a breakpoint in the VM's
  # kernel. Boots the VM, so it needs /dev/kvm.
  inspect =
    pkgs.runCommand "rewind-inspect"
      {
        nativeBuildInputs = [
          rewind
          gdb
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        ${listening}
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
        if rewind cat w $((end + 1)) /notes.txt 2> past; then exit 1; fi
        grep -q 'past its end' past

        # rewind shell, typed into from a pipe.
        printf 'cat /notes.txt; exit\n' | rewind shell w "$end" | tee shell
        grep -q second shell

        # rewind files: every entry under a directory, however deep, by
        # path, with a link's target and an empty directory. A file is not
        # a directory to list, a directory is not a file to print, and a
        # directory nothing has is missing.
        rewind run -q --epoch 1790985600 --name tree --root ${busyboxRoot} -- sh -c '
          mkdir -p /out/bin /out/empty
          echo hi > /out/bin/tool
          /bin/busybox chmod 755 /out/bin/tool
          /bin/busybox ln -s tool /out/bin/alias
          echo key > /out/secret
          /bin/busybox chmod 600 /out/secret'
        tree_end=$(rewind ls --json | sed -n 's/.*"name":"tree",.*"steps":\([0-9]*\).*/\1/p')
        rewind files tree "$tree_end" /out | tee files
        cat > want <<'EOF'
        drwxr-xr-x           0  bin
        lrwxrwxrwx           0  bin/alias -> tool
        -rwxr-xr-x           3  bin/tool
        drwxr-xr-x           0  empty
        -rw-------           4  secret
        EOF
        cmp files want
        rewind files tree "$tree_end" /out --json | tee files.json
        grep -qx '{"path":"bin/alias","kind":"symlink","mode":511,"size":0,"target":"tool"}' files.json
        status=0
        rewind files tree "$tree_end" /out/bin/tool || status=$?
        test "$status" = 4
        status=0
        rewind files tree "$tree_end" /nope || status=$?
        test "$status" = 3
        status=0
        rewind cat tree "$tree_end" /out || status=$?
        test "$status" = 4

        # rewind cat --archive tar: the directory whole, under its own
        # name, with the VM's permissions, owners and clock, which the
        # run's epoch set to 2026-10-03. Unpacked, the tree has the file,
        # the executable, the link and the empty directory, and a second
        # archive of the same step is the same bytes.
        rewind cat tree "$tree_end" /out --archive tar > out.tar
        tar --utc --numeric-owner -tvf out.tar | tee listed
        cat > want <<'EOF'
        drwxr-xr-x 0/0               0 2026-10-03 00:00 out/
        drwxr-xr-x 0/0               0 2026-10-03 00:00 out/bin/
        lrwxrwxrwx 0/0               0 2026-10-03 00:00 out/bin/alias -> tool
        -rwxr-xr-x 0/0               3 2026-10-03 00:00 out/bin/tool
        drwxr-xr-x 0/0               0 2026-10-03 00:00 out/empty/
        -rw------- 0/0               4 2026-10-03 00:00 out/secret
        EOF
        cmp listed want
        mkdir unpacked
        tar -x -C unpacked -f out.tar
        test "$(cat unpacked/out/bin/tool)" = hi
        test -x unpacked/out/bin/tool
        test "$(readlink unpacked/out/bin/alias)" = tool
        test -d unpacked/out/empty
        test -z "$(ls -A unpacked/out/empty)"
        test "$(stat -c %a unpacked/out/secret)" = 600
        test "$(cat unpacked/out/secret)" = key
        rewind cat tree "$tree_end" /out --archive tar | cmp - out.tar

        # A file alone is an archive of one name. A missing path fails
        # without emitting an archive header.
        rewind cat tree "$tree_end" /out/bin/tool --archive tar | tar -tf - | tee listed
        test "$(cat listed)" = tool
        status=0
        rewind cat tree "$tree_end" /nope --archive tar > missing || status=$?
        test "$status" = 3
        test ! -s missing

        # rewind gdb: a breakpoint where every event is reported.
        rewind gdb w "$start" --listen 127.0.0.1:12345 2> serve &
        server=$!
        listening $server
        gdb -q -batch -ex 'target remote 127.0.0.1:12345' \
          -ex 'break rewind_emit' -ex continue -ex detach \
          ${kernel}/vmlinux | tee gdb
        wait $server
        grep -q 'in rewind_emit' gdb

        # rewind gdb at a step inside the job: it names the process that
        # was running and loads its program, which in an image job comes
        # from the VM.
        write=$(rewind events w | grep 'notes.txt' | sed -n 1p)
        step=$(echo "$write" | awk '{print $1}')
        pid=$(echo "$write" | awk '{print $2}' | cut -d/ -f1)
        rewind gdb w "$step" --listen 127.0.0.1:12346 2> serve &
        server=$!
        listening $server
        gdb -q -batch -ex 'target remote 127.0.0.1:12346' -ex detach
        wait $server
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
        server=$!
        listening $server
        gdb -q -batch -ex 'target remote 127.0.0.1:12347' -ex detach
        wait $server
        echo "$write"
        cat serve
        grep -q "ran in process $pid;" serve
        touch $out
      '';

  # checks.in-rewind: derivations that `inRewind` rewrote build under a
  # stock nix-daemon, on the machine their attributes name. Each builder
  # checks known answers inside the VM: the date is the epoch's day,
  # 2026-10-03, whatever today is, the build has one core and the build
  # user's ids, and an attribute passed as a file arrives. The check then
  # reads what `rewind build` copied out: a file, an executable, a link and
  # an empty directory, and both outputs of a derivation with structured
  # attributes. The builder's own arguments reach the VM, bash's -e among
  # them, without which no failed answer would fail the build. A fixed-output derivation is refused when evaluated, and
  # `onRewind` changes the platform and adds the machine. The clock is exit
  # time, which needs no counter. Boots the VM, so it needs /dev/kvm.
  in-rewind =
    let
      inherit (pkgs) lib;
      machine = {
        epoch = 1790985600;
        clock = "exits";
      };

      # The machine as `rewindFlags` holds one.
      flags = "--epoch 1790985600 --seed 0 --schedule 0 --cpu v3-compat --clock exits --cores 1 --mem 1024";

      # CI advertises only `kvm`. These fixtures check vendor-independent
      # answers, so a test prelude selects the builder's actual vendor.
      # Production derivations keep the vendor selected at evaluation.
      selectVendor = pkgs.writeText "in-rewind-check-vendor.sh" ''
        case $(${lib.getExe pkgs.gawk} -F': ' '/^vendor_id/ { print $2; exit }' /proc/cpuinfo) in
          AuthenticAMD) vendor=amd ;;
          GenuineIntel) vendor=intel ;;
          *) echo "unsupported test CPU vendor" >&2; exit 1 ;;
        esac
        export rewindFlags="${flags} --vendor $vendor"
        exec "$@"
      '';

      inRewind =
        drv:
        let
          wrapped = rewindLib.inRewind machine drv;
        in
        assert wrapped.rewindFlags == "${flags} --vendor amd";
        assert wrapped.rewindRuntime == toString rewind;
        assert lib.elem "rewind-amd" wrapped.requiredSystemFeatures;
        lib.overrideDerivation wrapped (attrs: {
          requiredSystemFeatures = [ "kvm" ];
          args = [
            selectVendor
            pkgs.runtimeShell
          ]
          ++ attrs.args;
        });

      plain = inRewind (
        pkgs.runCommand "rewind-in-rewind-plain"
          {
            text = "passed as a file";
            passAsFile = [ "text" ];
          }
          ''
            test "$(date -u +%F)" = 2026-10-03
            test "$NIX_BUILD_CORES" = 1
            test "$(id -u):$(id -g)" = 1000:100
            test "$PWD" = /build
            test "$(cat "$textPath")" = "passed as a file"
            mkdir -p $out/bin $out/empty
            date -u +%F > $out/date
            echo "$-" > $out/shell-options
            echo '#!/bin/sh' > $out/bin/tool
            chmod +x $out/bin/tool
            ln -s tool $out/bin/alias
          ''
      );

      # A bare derivation omits `outputs`; Nix still gives the builder $out.
      bare = inRewind (
        builtins.derivation {
          name = "rewind-in-rewind-bare";
          system = "x86_64-linux";
          builder = pkgs.runtimeShell;
          args = [
            "-e"
            "-c"
            "echo bare > $out"
          ];
        }
      );

      structured = inRewind (
        pkgs.runCommand "rewind-in-rewind-structured"
          {
            __structuredAttrs = true;
            outputs = [
              "out"
              "doc"
            ];
          }
          ''
            mkdir -p $out $doc
            date -u +%F > $out/date
            echo read me > $doc/readme
          ''
      );

      platform = rewindLib.onRewind machine pkgs.hello;
      fetched = builtins.tryEval (rewindLib.inRewind machine pkgs.hello.src).drvPath;
    in
    assert !fetched.success;
    assert platform.system == "x86_64-rewind-amd";
    assert platform.rewindFlags == flags;
    assert platform.rewindRuntime == toString rewind;
    assert builtins.hasContext platform.rewindRuntime;
    assert platform.builder == pkgs.hello.builder;
    pkgs.runCommand "rewind-in-rewind" { } ''
      test "$(cat ${bare})" = bare
      test "$(cat ${plain}/date)" = 2026-10-03
      grep -q e ${plain}/shell-options
      test -x ${plain}/bin/tool
      test "$(readlink ${plain}/bin/alias)" = tool
      test -d ${plain}/empty
      test -z "$(ls -A ${plain}/empty)"
      test "$(cat ${structured}/date)" = 2026-10-03
      test "$(cat ${structured.doc}/readme)" = "read me"
      touch $out
    '';

  # checks.external-builder: Nix hands a build for the platform
  # x86_64-rewind-<vendor> to `rewind build --external-builder`, and takes
  # the output the command copied into the store. A Nix inside the check
  # builds a derivation for this machine's vendor in a store of its own,
  # with the program named in its external-builders setting; the builder,
  # a static busybox, writes the date, which is the epoch's day. The check
  # holds Nix's side of the handover: the description's form, and the line
  # Nix waits for before it reads the build's log. A CPU of neither vendor
  # skips the check and says so. Boots the VM, so it needs /dev/kvm.
  external-builder =
    let
      busybox = pkgs.pkgsStatic.busybox;
      closure = pkgs.closureInfo {
        rootPaths = [
          busybox
          rewind
        ];
      };
    in
    pkgs.runCommand "rewind-external-builder"
      {
        nativeBuildInputs = [
          rewind
          pkgs.nix
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        case $(awk -F': ' '/^vendor_id/ { print $2; exit }' /proc/cpuinfo) in
          AuthenticAMD) vendor=amd ;;
          GenuineIntel) vendor=intel ;;
          *)
            echo "skipped: this CPU is neither AMD's nor Intel's"
            touch $out
            exit 0
            ;;
        esac

        # A store of the check's own that holds the builder and runtime.
        export HOME=$TMPDIR/home
        store=$TMPDIR/store
        mkdir -p $store/nix/store
        xargs -I{} cp -a {} $store/nix/store/ < ${closure}/store-paths
        nix-store --store $store --load-db < ${closure}/registration

        cat > day.nix <<EOF
        { runtime ? builtins.storePath ${rewind} }:
        derivation {
          name = "day";
          system = "x86_64-rewind-$vendor";
          builder = "\''${builtins.storePath ${busybox}}/bin/busybox";
          args = [ "sh" "-c" "echo the builder ran; \''${builtins.storePath ${busybox}}/bin/busybox date -u +%F > \\\$out" ];
          rewindFlags = "--epoch 1790985600 --cpu v3-compat --clock exits";
          rewindRuntime = runtime;
        }
        EOF
        build_day() {
          nix-build day.nix --store $store --no-out-link \
            --extra-experimental-features external-builders \
            --external-builders '[{"systems":["x86_64-rewind-'$vendor'"],"program":"${rewind}/bin/rewind","args":["build","--external-builder"]}]' \
            --option substituters "" "$@"
        }

        # Shell overrides cannot replace the guest pinned by the runtime.
        built=$(REWIND_KERNEL=/missing-kernel REWIND_INITRD=/missing-initrd \
          build_day 2> log) || { cat log; exit 1; }
        cat log
        grep -q 'the builder ran' log
        test "$(cat $store$built)" = 2026-10-03

        # A different package changes the recipe and fails before its job.
        if build_day --argstr runtime ${busybox} > mismatch-out 2> mismatch; then
          echo "accepted a different runtime" >&2
          exit 1
        fi
        cat mismatch
        grep -q 'requires Rewind package' mismatch
        grep -q 'configured builder is ${rewind}' mismatch
        if grep -q 'the builder ran' mismatch; then exit 1; fi
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
          gdb
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

        # A session at a step before a process's exec, as a forked child's
        # first context switch is, stops at the exec and refuses to run into
        # the new program; those are skipped.
        sessions=0
        for step in $(seq "$start" 4 "$end"); do
          rewind gdb busy "$step" -- -batch -ex 'stepi 3000' -ex continue > gdb 2>&1 || true
          if grep -q 'debugs the program before it' gdb; then
            continue
          fi
          follows "stepping from step $step"
          sessions=$((sessions + 1))
        done
        test "$sessions" -gt 5

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
          gdb
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
        test "$(grep -c 'New value = 1' gdb)" = 0
        grep -q 'exited normally' gdb
        test "$(grep -c 'left the recording\|SIGTRAP' gdb)" = 0

        # The child's write is passed, the parent's last one stops gdb.
        rewind gdb watch "$start" -- -batch \
          -ex 'break write' -ex continue -ex 'print counter' -ex delete -ex continue > break 2>&1 || true
        cat break
        grep -q '^\$1 = 3' break
        grep -q 'exited normally' break
        test "$(grep -c 'left the recording\|SIGTRAP' break)" = 0

        rewind gdb watch "$start" -- -batch -ex 'rwatch counter' -ex continue > read 2>&1 || true
        cat read
        grep -q 'Could not insert' read
        touch $out
      '';

  # checks.blame-memory: `rewind blame RUN --at STEP EXPR` names who last
  # wrote a value in memory. At the program's last write, counter holds 3,
  # last written by main at watch.c:16 (counter = 3), and 2 before that,
  # at watch.c:15, with the process's files loaded into gdb once for both
  # of its sessions. The child's write of 1, at the same address in its own
  # memory, is no write of the parent's: two writes back, the search finds
  # only the parent's two. --address names the same bytes for programs,
  # and its search says how far it got in JSON lines, one of them as it
  # goes on to name the lines. A write thousands of steps back is found
  # through windows restored in place, and `rewind where --after-write`
  # just after each write names the write's line. One store over two
  # watched values writes both; a value's last write stays found behind
  # 20,000 writes of another; and a REP MOVSB that reads a byte, then
  # writes it, at one instruction, lists the read before the write. A
  # write a library made in the step that loaded it is named by that
  # library. So is one it made before it was unloaded, another library
  # loaded at its addresses since; the files are loaded once only when the
  # loader's list is the same at both steps. Boots the VM, so it needs
  # /dev/kvm.
  blame-memory =
    pkgs.runCommand "rewind-blame-memory"
      {
        nativeBuildInputs = [
          rewind
          gdb
          pkgs.jq
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        # No debuginfod server: without a network, each of gdb's questions
        # to it waits out a timeout.
        export REWIND_DEBUGINFOD=/nonexistent

        rewind run -q --clock exits --name watch --root ${gdbRoot} -- /bin/watch
        done=$(rewind events watch | grep 'write(1, "done' | awk '{print $1}')

        # One gdb names the value at the step and places the write at its
        # own step: the process's files are read and loaded once.
        rewind blame watch --at "$done" counter > blame 2> blame.err
        cat blame blame.err
        test "$(grep -c 'loading symbols for' blame.err)" = 1
        grep -q '^counter = 3   4 bytes at 0x' blame
        grep -q '^last written at step [0-9]* by thread [0-9]* (watch)$' blame
        grep -q '^  watch.c:16  main$' blame
        grep -q '^  was 2$' blame

        # The parent's two writes, not the child's.
        rewind blame watch --at "$done" counter --writes 3 --json > blame.json
        jq -e '.accesses | map(.after) == ["2", "3"]
          and map(.place.line) == [15, 16] and all(.kind == "write")' blame.json
        jq -e '.values[0] | .size == 4 and .class == "signed" and .initial == "0"' blame.json

        # The same bytes by address, with the search's progress.
        address=$(jq -r '.values[0].address' blame.json)
        pid=$(jq -r '.pid' blame.json)
        rewind --progress json blame watch --at "$done" --address "$address" --size 4 \
          --class signed --pid "$pid" --json > address.json 2> progress
        cat progress
        jq -e '.accesses[-1] | .after == "3" and .place.line == 16' address.json
        grep -q '^{"searching":{"phase":"searching"' progress
        test "$(grep -c '"phase":"naming"' progress)" = 1

        # Just after each write blame names, where names the write's line,
        # where the step's start can be lines before it.
        for i in 0 1; do
          access=$(jq -c ".accesses[$i]" blame.json)
          rewind where watch "$(jq -r .step <<< "$access")" --pid "$pid" \
            --tid "$(jq -r .tid <<< "$access")" --after-write "$address" --size 4 \
            --hit "$(jq -r .hit <<< "$access")" --json > after.json
          jq -e --argjson line "$(jq .place.line <<< "$access")" \
            '.frames[.chosen].line == $line' after.json
        done

        # A write 30,000 steps back, past two keyframes about 12,800
        # steps apart: the windows after the first hold no write, and
        # each puts the machine at its own keyframe in place. The value is
        # 0 where the search began, before the write.
        rewind run -q --clock exits --name late --root ${gdbRoot} -- /bin/late > /dev/null
        done=$(rewind events late | grep 'write(1, "done' | awk '{print $1}')
        rewind blame late --at "$done" counter --json > late.json
        jq -c '{windows, searched_from, step: .accesses[-1].step}' late.json
        jq -e '.windows >= 3 and .accesses[-1].after == "7"
          and .accesses[-1].place.line == 7 and .values[0].initial == "0"' late.json

        # One 64-bit store writes both halves of the pair at line 19, and
        # line 20 writes the high half again: each half has its own
        # writes, with the bytes on both sides of each.
        rewind run -q --clock exits --name fields --root ${gdbRoot} -- /bin/fields > /dev/null
        done=$(rewind events fields | grep 'write(1, "done' | awk '{print $1}')
        rewind blame fields --at "$done" pair.fields.low pair.fields.high --writes 2 \
          --json > fields.json
        jq -c '.accesses | map([.value, .before, .after, .place.line])' fields.json
        jq -e '.accesses | map([.value, .before, .after, .place.line])
          == [[0, "0", "1", 19], [1, "0", "2", 19], [1, "2", "5", 20]]' fields.json

        # 20,000 writes of the counter after the high half's last write
        # leave each value's last write; a cap on --reads says what it
        # left out.
        rewind blame fields --at "$done" pair.fields.high counter > stress
        cat stress
        grep -q '^  fields.c:20  main' stress
        grep -q '^  fields.c:22  main' stress
        test "$(grep -c 'not written' stress)" = 0
        rewind blame fields --at "$done" counter --reads --max 1 --json > capped.json
        jq -e '.listed == "accesses" and (.accesses | length) == 1 and .dropped > 0' capped.json

        # REP MOVSB at line 23 reads buffer[2] in one iteration and writes
        # it in the next, at the same instruction: a read of 3, then a
        # write of 3 to 4. Just after the read, where names its line.
        rewind blame fields --at "$done" 'buffer[2]' --reads --json > rep.json
        jq -c '.accesses | map([.kind, .before, .after])' rep.json
        jq -e '.accesses[-2:] | map([.kind, .before, .after, .place.line])
          == [["read", "3", "3", 23], ["write", "3", "4", 23]]' rep.json
        read=$(jq -c '.accesses[-2]' rep.json)
        rewind where fields "$(jq -r .step <<< "$read")" --pid "$(jq -r .pid rep.json)" \
          --tid "$(jq -r .tid <<< "$read")" --after-access "$(jq -r '.values[0].address' rep.json)" \
          --size 1 --hit "$(jq -r .hit <<< "$read")" --json > after-read.json
        jq -e '.frames[.chosen].line == 23' after-read.json

        # actual_writer in first.so wrote the total in the step that loaded
        # first.so, so the files are read just after the write, where
        # first.so is mapped, when blame names the bytes by address and
        # when `rewind where --after-write` looks there.
        rewind run -q --clock exits --name reload --root ${reloadRoot} -- /bin/reload
        rewind events reload > reload
        grep -q 'write(1, "same address' reload
        written=$(grep 'write(1, "written' reload | awk '{print $1}')
        reloaded=$(grep 'write(1, "reloaded' reload | awk '{print $1}')
        rewind blame reload --at "$written" total --json > written.json
        address=$(jq -r '.values[0].address' written.json)
        pid=$(jq -r '.pid' written.json)
        write=$(jq -c '.accesses[-1]' written.json)
        rewind blame reload --at "$written" --address "$address" --size 4 --class signed \
          --pid "$pid" > by-address
        cat by-address
        grep -q '^  first.c:3  actual_writer  \*value = 7;$' by-address
        rewind where reload "$(jq -r .step <<< "$write")" --pid "$pid" \
          --tid "$(jq -r .tid <<< "$write")" --after-write "$address" --size 4 \
          --hit "$(jq -r .hit <<< "$write")" --json > after-load.json
        jq -e '.frames[0] | .function == "actual_writer" and .line == 3' after-load.json

        # second.so took first.so's place after the write: blamed there,
        # the write is still first.c's, placed with the files read again
        # at its own step. Blamed before the unload, one load of the files
        # serves both steps.
        rewind blame reload --at "$reloaded" total > reloaded 2> reloaded.err
        cat reloaded reloaded.err
        grep -q '^total = 7   4 bytes at 0x' reloaded
        grep -q '^  first.c:3  actual_writer  \*value = 7;$' reloaded
        test "$(grep -c 'never_called\|second.c' reloaded)" = 0
        test "$(grep -c 'loading symbols for' reloaded.err)" = 2
        rewind blame reload --at "$written" total > written 2> written.err
        cat written written.err
        grep -q '^  first.c:3  actual_writer  \*value = 7;$' written
        test "$(grep -c 'loading symbols for' written.err)" = 1
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
  # cannot, or is not exact, as on AMD Zen when the sandbox lacks the
  # workaround's marker, skips the check and says so. Boots the VM, so it
  # needs /dev/kvm.
  gdb-cold =
    pkgs.runCommand "rewind-gdb-cold"
      {
        nativeBuildInputs = [
          rewind
          gdb
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
          grep -q -e 'perf_event_open' -e 'needs an exact branch counter' run
          echo "skipped: this machine's branch counter cannot drive virtual time"
          touch $out
          exit 0
        fi
        start=$(rewind events busy | grep 'rewind-start' | sed -n 1p | awk '{print $1}')
        end=$(rewind events busy | grep 'rewind-exit' | sed -n 1p | awk '{print $1}')

        # A step at which a process runs before its exec, as a forked child's
        # first context switch is, gives a session that stops at the exec
        # and refuses to run into the new program; those are skipped.
        sessions=0
        skipped=0
        for step in $(seq "$start" 3 "$end"); do
          rewind gdb busy "$step" -- -batch -ex continue > gdb 2>&1 || true
          if grep -q 'debugs the program before it' gdb; then
            skipped=$((skipped + 1))
            continue
          fi
          if ! grep -q 'exited normally' gdb || grep -q 'left the recording\|SIGTRAP' gdb; then
            echo "from step $step:"
            cat gdb
            exit 1
          fi
          sessions=$((sessions + 1))
        done
        test "$sessions" -gt 10
        echo "$sessions forks for gdb, each made at a step without a keyframe, stayed on the recording; $skipped stopped at an exec"
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
        test "$(grep -c '^State:.*zombie' status)" = 0
        rewind cat crash "$step" /proc/$pid/task/$tid/stat > stat
        cat stat
        test "$(awk '{print $3}' stat)" = T
        touch $out
      '';

  # checks.switches: the guest records every context switch, and `rewind
  # threads` reads which task held the CPU from those records, with
  # nothing replayed. Three workers wait on a futex while the main thread
  # sleeps a second, then prints and wakes them. The slices cover the
  # window without a gap, the sleep shows as idle, each worker holds the
  # CPU after the print, and the answer comes with the run's keyframes
  # gone, which a replay would need. The switches are not events, and a
  # replay compares them. Boots the VM, so it needs /dev/kvm.
  switches =
    pkgs.runCommand "rewind-switches"
      {
        nativeBuildInputs = [
          rewind
          pkgs.jq
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        rewind run -q --clock exits --name threads --root ${gdbRoot} -- /bin/threads
        write=$(rewind events threads | grep 'write(1, "open')
        open=$(echo "$write" | awk '{print $1}')
        pid=$(echo "$write" | awk '{print $2}' | cut -d/ -f1)
        last=$(rewind ls --json | jq -s '.[0].steps')

        # The switches are records of the run, compared by a replay from
        # boot and by one from a keyframe, whose first batch holds the
        # switches pending at the keyframe, and not events. Encoded, they
        # take a few bytes each.
        rewind replay threads | tee replay
        grep -q 'identical to the recording: .* context switches' replay
        rewind replay threads --from 300 | tee replay-from
        grep -q 'identical to the recording from the snapshot' replay-from
        test "$(rewind events threads | grep -c 'record kind 1[45]')" = 0
        switches=$(rewind threads threads --json | wc -l)
        size=$(stat -c %s $REWIND_HOME/runs/*/trace.bin)
        echo "$switches slices, $size bytes of trace"
        test "$size" -lt $((switches * 8 + 65536))

        # No keyframe is left to replay from: the answer is read.
        rm -rf $REWIND_HOME/runs/*/keyframes
        rewind threads threads --json > slices
        test "$(wc -l < slices)" -gt 10

        # Contiguous: each slice starts the step after the one before.
        jq -s '[range(1; length) as $i | select(.[$i].from != .[$i - 1].to + 1)] | length' slices \
          | grep -qx 0
        test "$(jq -s -r '.[-1].to' slices)" = "$last"

        # The main thread, idle while it sleeps, and each worker afterwards.
        test -n "$(jq -c "select(.on == \"thread\" and .pid == $pid and .tid == $pid)" slices)"
        test -n "$(jq -c "select(.on == \"idle\" and .to < $open)" slices)"
        workers=$(jq -s -r "map(select(.on == \"thread\" and .pid == $pid and .tid != $pid and .from > $open) | .tid) | unique | length" slices)
        test "$workers" = 3

        # A window inside the run is the same slices, clipped to it.
        rewind threads threads --from $((open - 5)) --to $((open + 5)) --json > window
        test "$(jq -s -r '.[0].from' window)" = $((open - 5))
        test "$(jq -s -r '.[-1].to' window)" = $((open + 5))
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
          gdb
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        # No debuginfod server: without a network, each of gdb's questions
        # to it waits out a timeout.
        export REWIND_DEBUGINFOD=/nonexistent
        ${listening}

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
          server=$!
          listening $server
          gdb -q -batch -ex 'target remote 127.0.0.1:12350' -ex detach > /dev/null 2>&1
          wait $server
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
          gdb
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
          test "$(grep -c 'left the recording\|SIGTRAP\|no debug register' gdb)" = 0
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
        test "$(grep -c '^0x[0-9a-f]* <sixth>:.*0xcc' kernel)" = 0
        grep -q 'exited normally' kernel
        test "$(grep -c 'left the recording' kernel)" = 0
        touch $out
      '';

  # checks.gdb-reverse: gdb runs a fork backwards, under exit time and,
  # where the branch counter opens, counter time. From the program's last
  # write, after five adds to a total, a watch on the total run backwards
  # stops before each add, with the values swapped, the last add first.
  # A breakpoint run backwards to stops in the fourth add, reverse-finish
  # goes back to the line that called it, and reverse-next, with the
  # breakpoint gone, through the lines before it into the round before. A reverse-stepi then a stepi land where they began.
  # With the watch and two breakpoints set, reverse-next goes back over
  # the dot's write() to the line before, the four debug registers held.
  # Run forwards again, the fork stops at the next add, the third, and
  # ends as the run did, on its recording. With nothing to stop at, a run backwards
  # reaches the program's exec, where its memory began. A watch on a buffer
  # the kernel fills in read() stops, run backwards, at the read's syscall
  # instruction in the process's thread, and run forwards, once, after the
  # read returns, in the same thread. A watch on a buffer recv() fills
  # stops forwards at the first instruction the thread runs in user space
  # after the kernel's copy, a signal handler's, whether the handler
  # returns, exits or jumps out; a breakpoint another thread hits on the
  # way stops after the watch, and runs forwards and back from there stop
  # the same each time. So does another thread's write to a second watch,
  # after the kernel's. Two watches the call writes stop once each, and a
  # thread a signal kills in the call stops as it exits. A watch on a 1 MiB
  # read() stops in seconds. A reverse-stepi from the start of the step
  # of a record the kernel made in write() or exit_group() stops in the
  # kernel, at the out that made the record, though the step before
  # entered the kernel by a syscall instruction, by a page fault in user
  # space, or by an invalid opcode or a privileged instruction there;
  # steps back from the kernel's exception entry go through the invalid
  # opcode's IDT stub, one instruction at a time, to the ud2, steps
  # forwards from it the same way back, and the
  # process's thread shows the registers it faulted with while the kernel
  # saves them.
  # Stepped through write()'s system call, the process's thread shows the
  # pc and sp it made the call with until the call returns. A run
  # forwards stops before the
  # process's exec, whose new program it never runs into, says so, and
  # refuses to go on, a stepi 100 once; a step back from there stops at
  # the out before it,
  # in the CPU's thread. A kernel breakpoint between the exec's switch to
  # the new page table and its record does not stop it. A session at the
  # step the message names stops in the new program. A Ctrl-C in a step back across 10 million
  # rounds of a loop stops it there, where gdb goes on. A Ctrl-C while a
  # continue goes back to the stop before an exec is answered by that
  # stop, and does not stop gdb's next command. Boots the VM, so it needs
  # /dev/kvm.
  gdb-reverse =
    pkgs.runCommand "rewind-gdb-reverse"
      {
        nativeBuildInputs = [
          rewind
          pkgs.gdb
          pkgs.procps
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        # No debuginfod server: without a network, each of gdb's questions
        # to it waits out a timeout.
        export REWIND_DEBUGINFOD=/nonexistent

        # timeout's exit status for a session it stopped with TERM, and
        # for one it then had to KILL. gdb -batch exits nonzero on its
        # own when its last command fails, which the sessions below allow.
        timeout_term=124
        timeout_kill=137

        for clock in exits branches; do
          if ! rewind run -q --clock "$clock" --name "reverse-$clock" --root ${gdbRoot} \
            -- /bin/reverse > run 2>&1; then
            cat run
            grep -q 'perf_event_open' run
            echo "skipped counter time: this machine's branch counter cannot drive it"
            continue
          fi
          done=$(rewind events "reverse-$clock" | grep 'write(1, "done' | awk '{print $1}')

          rewind gdb "reverse-$clock" "$done" -- -batch \
            -ex 'watch total' -ex reverse-continue -ex reverse-continue \
            -ex delete -ex 'break add' -ex reverse-continue -ex 'print n' \
            -ex reverse-finish -ex delete -ex reverse-next -ex reverse-next -ex reverse-next \
            -ex 'print i' -ex 'print $pc' -ex reverse-stepi -ex stepi -ex 'print $pc' \
            -ex 'break add' -ex continue -ex 'print n' -ex delete -ex continue > gdb 2>&1 || true
          cat gdb

          # The watch, backwards: before the fifth add, then the fourth.
          test "$(grep -c 'hit Hardware watchpoint 1: total' gdb)" = 2
          test "$(grep -A1 'Old value = 15' gdb | grep -c 'New value = 10')" = 1
          test "$(grep -A1 'Old value = 10' gdb | grep -c 'New value = 6')" = 1
          grep -q 'in add (n=5) at .*reverse.c:7' gdb
          grep -q 'in add (n=4) at .*reverse.c:7' gdb

          # The breakpoint backwards, out of add to its call, and back
          # through the lines before into the third round.
          grep -q 'hit Breakpoint 2, add (n=4)' gdb
          grep -q '^\$1 = 4$' gdb
          grep -q 'in main () at .*reverse.c:14' gdb
          grep -q 'the call' gdb
          grep -q 'the dot' gdb
          grep -q '^\$2 = 3$' gdb

          # A step back and forward lands where it began.
          grep -E '^\$[34] = \(void' gdb > pcs
          test "$(wc -l < pcs)" = 2
          test "$(awk '{print $5}' pcs | sort -u | wc -l)" = 1

          # Forwards again: the next add is the third, and the run ends.
          grep -q 'hit Breakpoint 3, add (n=3)' gdb
          grep -q '^\$5 = 3$' gdb
          grep -q 'exited normally' gdb
          test "$(grep -c 'left the recording\|SIGTRAP' gdb)" = 0

          # A reverse-next back over the dot's write(), with the watch and
          # two breakpoints set: gdb's breakpoint at write's start takes
          # the fourth debug register, and the single steps back from the
          # kernel's return to user space still stop there. The line before
          # is the dot, in the fourth round, and the watch does not stop,
          # since nothing writes the total in between. A session that hangs
          # is cut short, and fails.
          status=0
          timeout -k 5 300 rewind gdb "reverse-$clock" "$done" -- -batch \
            -ex 'watch total' -ex 'break exit' -ex 'break __libc_start_main' -ex reverse-continue \
            -ex reverse-finish -ex reverse-next -ex reverse-next -ex 'print i' \
            > crossed 2>&1 || status=$?
          cat crossed
          test "$status" != $timeout_term
          test "$status" != $timeout_kill
          test "$(grep -c '^Breakpoint [23] at' crossed)" = 2
          test "$(grep -c 'hit Hardware watchpoint 1: total' crossed)" = 1
          grep -q '^15[[:space:]].*the dot' crossed
          grep -q '^\$1 = 4$' crossed
          test "$(grep -c 'fatal error\|Remote connection closed\|target is running' crossed)" = 0

          # Nothing to stop at: back to the program's exec.
          rewind gdb "reverse-$clock" "$done" -- -batch -ex reverse-continue > start 2>&1 || true
          cat start
          grep -q 'Reached end of recorded history' start
          test "$(grep -c 'only the CPU' start)" = 0

          # Five writes to the watch in one step, with no exit between:
          # each run back stops before the write before the last one. A
          # sixth run back reaches the program's exec, where its memory
          # began, and not the kernel's exec zeroing the total before it.
          rewind run -q --clock "$clock" --name "burst-$clock" --root ${gdbRoot} \
            -- /bin/burst > /dev/null
          done=$(rewind events "burst-$clock" | grep 'write(1, "done' | awk '{print $1}')
          rewind gdb "burst-$clock" "$done" -- -batch -ex 'watch total' \
            -ex reverse-continue -ex reverse-continue -ex reverse-continue \
            -ex reverse-continue -ex reverse-continue -ex reverse-continue \
            -ex 'print total' > burst 2>&1 || true
          cat burst
          test "$(grep -c 'hit Hardware watchpoint 1: total' burst)" = 5
          test "$(grep -A1 'Old value = 15' burst | grep -c 'New value = 10')" = 1
          test "$(grep -A1 'Old value = 10' burst | grep -c 'New value = 6')" = 1
          test "$(grep -A1 'Old value = 6' burst | grep -c 'New value = 3')" = 1
          test "$(grep -A1 'Old value = 3$' burst | grep -c 'New value = 1')" = 1
          test "$(grep -A1 'Old value = 1$' burst | grep -c 'New value = 0')" = 1
          grep -q 'Reached end of recorded history' burst
          grep -q '^\$1 = 0$' burst
          test "$(grep -c 'unreadable' burst)" = 0

          # The kernel fills the buffer in read(), byte by byte, after the
          # child that writes the pipe has slept, so other tasks ran
          # between the call and the writes. A run back to a watch on the
          # buffer stops once, in the process's thread at its syscall
          # instruction, with the buffer as it was before the call, and
          # not in the kernel's copy. The next run back reaches the
          # program's exec.
          rewind run -q --clock "$clock" --name "piped-$clock" --root ${gdbRoot} \
            -- /bin/piped > /dev/null
          done=$(rewind events "piped-$clock" | grep 'write(1, "done' | awk '{print $1}')
          rewind gdb "piped-$clock" "$done" -- -batch -ex 'watch buffer' -ex reverse-continue \
            -ex 'x/i $pc' -ex bt -ex reverse-continue -ex 'print buffer' > piped 2>&1 || true
          cat piped
          test "$(grep -c 'hit Hardware watchpoint 1: buffer' piped)" = 1
          grep -q 'Thread 2 hit Hardware watchpoint 1: buffer' piped
          test "$(grep -A1 'Old value = "hello' piped | grep -c 'New value = "\(.000\)\{7\}"$')" = 1
          grep -q '^=> 0x[0-9a-f]* <[^>]*>:[[:space:]]*syscall' piped
          grep -q 'in main () at .*piped.c:18' piped
          test "$(grep -c 'rep_movs\|copy_user' piped)" = 0
          grep -q 'Reached end of recorded history' piped
          grep -q '^\$1 = "\(.000\)\{7\}"$' piped

          # Run forwards from before the read, the same watch stops once,
          # in the process's thread at the instruction after its syscall
          # instruction, once read() has returned, with the whole buffer
          # written. The next continue runs to the run's end.
          start=$(rewind events "piped-$clock" | grep 'write(1, "start' | awk '{print $1}')
          status=0
          timeout 300 rewind gdb "piped-$clock" "$start" -- -batch -ex 'watch buffer' \
            -ex continue -ex 'x/2xb $pc-2' -ex bt -ex 'print buffer' -ex continue \
            > piped-forward 2>&1 || status=$?
          cat piped-forward
          test "$status" != $timeout_term
          test "$status" != $timeout_kill
          test "$(grep -c 'hit Hardware watchpoint 1: buffer' piped-forward)" = 1
          grep -q 'Thread 2 hit Hardware watchpoint 1: buffer' piped-forward
          test "$(grep -A1 'Old value = "\(.000\)\{7\}"$' piped-forward | grep -c 'New value = "hello')" = 1
          grep -q '^0x[0-9a-f]* <[^>]*>:[[:space:]]*0x0f[[:space:]]*0x05$' piped-forward
          grep -q 'in main () at .*piped.c:18' piped-forward
          test "$(grep -c 'rep_movs\|copy_user' piped-forward)" = 0
          grep -q '^\$1 = "hello' piped-forward

          # A watch on the buffer recv() fills, run forwards from before
          # the call: the kernel copies 5 bytes, waits on for the rest, and
          # returns them into the signal handler once the second thread
          # signals. The watch stops once, in the main thread at the
          # handler's first instruction, the first it runs in user space
          # after the copy, with "hello" in the buffer, and the breakpoint
          # in the handler stops next. The handler's own write() returns
          # from the instruction the recv would, and is not the stop. A
          # handler that _exits, or siglongjmps back into main, and never
          # returns, stops the same. A session that hangs is cut short, and
          # fails.
          for mode in r e j; do
            rewind run -q --clock "$clock" --name "handled-$mode-$clock" --root ${gdbRoot} \
              -- /bin/handled "$mode" > /dev/null
            start=$(rewind events "handled-$mode-$clock" | grep 'write(1, "start' | awk '{print $1}')
            status=0
            timeout -k 5 300 rewind gdb "handled-$mode-$clock" "$start" -- -batch \
              -ex 'watch buffer' -ex 'break handler' -ex continue -ex bt -ex continue \
              -ex continue > "handled-$mode" 2>&1 || status=$?
            cat "handled-$mode"
            test "$status" != $timeout_term
            test "$status" != $timeout_kill
            test "$(grep -c 'hit Hardware watchpoint 1: buffer' "handled-$mode")" = 1
            grep -A4 'Thread 2 hit Hardware watchpoint 1: buffer' "handled-$mode" > stop
            grep -q 'New value = "hello' stop
            grep -q '^handler (sig=[0-9]*) at .*handled.c:16$' stop
            grep -q '<signal handler called>' "handled-$mode"
            grep -q 'Thread 2 hit Breakpoint 2, handler (sig=10)' "handled-$mode"
            test "$(grep -c 'in write ()\|rep_movs\|copy_user' "handled-$mode")" = 0
          done

          # With a breakpoint in the second thread, at the call that
          # signals, the watch is told of when the second thread reaches
          # it, first, in the main thread at the instruction after its
          # syscall instruction, where it waits in the kernel; the
          # breakpoint stops next, at once, then the handler's. Run back,
          # forwards and back again, the same stops come in the same order,
          # and two runs back from the second thread's breakpoint reach the
          # recv's syscall instruction, with the buffer as before the call.
          start=$(rewind events "handled-r-$clock" | grep 'write(1, "start' | awk '{print $1}')
          status=0
          timeout -k 5 300 rewind gdb "handled-r-$clock" "$start" -- -batch -ex 'watch buffer' \
            -ex 'break signal_reader' -ex 'break handler' -ex continue -ex 'x/2xb $pc-2' \
            -ex continue -ex continue -ex reverse-continue -ex continue -ex reverse-continue \
            -ex reverse-continue -ex 'x/i $pc' -ex continue -ex continue \
            > handled-other 2>&1 || status=$?
          cat handled-other
          test "$status" != $timeout_term
          test "$status" != $timeout_kill
          # The second thread exits once it has signaled, so a stop in the
          # main thread after that lists it no more, and a run back brings
          # it back under a new gdb number; the stops are compared by which
          # of the two threads they are in.
          grep 'hit ' handled-other | sed 's/,.*//; s/^Thread [3-9][0-9]* hit/Thread other hit/' > hits
          {
            echo 'Thread 2 hit Hardware watchpoint 1: buffer'
            echo 'Thread other hit Breakpoint 2'
            echo 'Thread 2 hit Breakpoint 3'
            echo 'Thread other hit Breakpoint 2'
            echo 'Thread 2 hit Breakpoint 3'
            echo 'Thread other hit Breakpoint 2'
            echo 'Thread 2 hit Hardware watchpoint 1: buffer'
            echo 'Thread 2 hit Hardware watchpoint 1: buffer'
            echo 'Thread other hit Breakpoint 2'
          } > expected
          diff expected hits
          grep -q '^0x[0-9a-f]* <[^>]*>:[[:space:]]*0x0f[[:space:]]*0x05$' handled-other
          grep -q '^=> 0x[0-9a-f]* <[^>]*>:[[:space:]]*syscall' handled-other
          test "$(grep -A1 'Old value = "hello' handled-other | grep -c 'New value = .\\000. <repeats 15 times>$')" = 1
          test "$(grep -c 'rep_movs\|copy_user' handled-other)" = 0

          # A watch on the buffer's first byte, and one on the global the
          # second thread writes while the main thread waits in the
          # kernel: the kernel's write is told of first, when the second
          # thread writes, in the main thread, then the second thread's
          # write, then the handler's breakpoint. Run back, the second
          # thread's write stops, then the kernel's, at the recv's syscall
          # instruction, and forwards again, the two in the same order.
          status=0
          timeout -k 5 300 rewind gdb "handled-r-$clock" "$start" -- -batch \
            -ex 'watch buffer[0]' -ex 'watch other' -ex 'break handler' -ex continue \
            -ex continue -ex continue -ex reverse-continue -ex reverse-continue -ex 'x/i $pc' \
            -ex continue -ex continue > handled-second 2>&1 || status=$?
          cat handled-second
          test "$status" != $timeout_term
          test "$status" != $timeout_kill
          # The second thread's gdb number changes as above.
          grep 'hit ' handled-second | sed 's/,.*//; s/^Thread [3-9][0-9]* hit/Thread other hit/' > hits
          {
            echo 'Thread 2 hit Hardware watchpoint 1: buffer[0]'
            echo 'Thread other hit Hardware watchpoint 2: other'
            echo 'Thread 2 hit Breakpoint 3'
            echo 'Thread other hit Hardware watchpoint 2: other'
            echo 'Thread 2 hit Hardware watchpoint 1: buffer[0]'
            echo 'Thread 2 hit Hardware watchpoint 1: buffer[0]'
            echo 'Thread other hit Hardware watchpoint 2: other'
          } > expected
          diff expected hits
          grep -q '^=> 0x[0-9a-f]* <[^>]*>:[[:space:]]*syscall' handled-second
          test "$(grep -A1 'Old value = 0$' handled-second | grep -c 'New value = 1$')" = 2
          test "$(grep -A1 'Old value = 1$' handled-second | grep -c 'New value = 0$')" = 1

          # Watches on two bytes the one recv() writes: each is told of,
          # one stop each, at the handler's first instruction, and the
          # handler's breakpoint stops after them. Run back, the two stop
          # again, one stop each, at the recv's syscall instruction, and
          # forwards again, at the handler's first instruction.
          status=0
          timeout -k 5 300 rewind gdb "handled-r-$clock" "$start" -- -batch \
            -ex 'watch buffer[0]' -ex 'watch buffer[4]' -ex 'break handler' -ex continue \
            -ex continue -ex continue -ex reverse-continue -ex reverse-continue -ex 'x/i $pc' \
            -ex continue -ex continue > handled-two 2>&1 || status=$?
          cat handled-two
          test "$status" != $timeout_term
          test "$status" != $timeout_kill
          grep 'hit ' handled-two | sed 's/,.*//' > hits
          {
            echo 'Thread 2 hit Hardware watchpoint 1: buffer[0]'
            echo 'Thread 2 hit Hardware watchpoint 2: buffer[4]'
            echo 'Thread 2 hit Breakpoint 3'
            echo 'Thread 2 hit Hardware watchpoint 2: buffer[4]'
            echo 'Thread 2 hit Hardware watchpoint 1: buffer[0]'
            echo 'Thread 2 hit Hardware watchpoint 1: buffer[0]'
            echo 'Thread 2 hit Hardware watchpoint 2: buffer[4]'
          } > expected
          diff expected hits
          test "$(grep -c '^handler (sig=[0-9]*) at .*handled.c:16$' handled-two)" = 4
          grep -q '^=> 0x[0-9a-f]* <[^>]*>:[[:space:]]*syscall' handled-two
          test "$(grep -c "New value = 104 'h'" handled-two)" = 2
          test "$(grep -c "New value = 111 'o'" handled-two)" = 2
          test "$(grep -A1 "Old value = 104 'h'" handled-two | grep -c "New value = 0 ")" = 1
          test "$(grep -A1 "Old value = 111 'o'" handled-two | grep -c "New value = 0 ")" = 1

          # With no handler, the signal kills the process in recv(), whose
          # call never returns: the watch is told of as the main thread
          # exits, in that thread, with "hello" in the buffer, and the run
          # goes on to its end.
          status=0
          rewind run -q --clock "$clock" --name "handled-k-$clock" --root ${gdbRoot} \
            -- /bin/handled k > /dev/null || status=$?
          test "$status" = 138
          start=$(rewind events "handled-k-$clock" | grep 'write(1, "start' | awk '{print $1}')
          status=0
          timeout -k 5 300 rewind gdb "handled-k-$clock" "$start" -- -batch -ex 'watch buffer' \
            -ex continue -ex bt -ex continue > handled-k 2>&1 || status=$?
          cat handled-k
          test "$status" != $timeout_term
          test "$status" != $timeout_kill
          test "$(grep -c 'hit Hardware watchpoint 1: buffer' handled-k)" = 1
          grep -A3 'Thread 2 hit Hardware watchpoint 1: buffer' handled-k > stop
          grep -q 'New value = "hello' stop
          grep -q 'in main (.*) at .*handled.c' handled-k

          # A watch on the first byte of a 1 MiB read() stops once, after
          # the read returns, at the instruction after its syscall
          # instruction, in under 5 seconds: the kernel copies the rest at
          # full speed, not a single step at a time, which took 14.
          rewind run -q --clock "$clock" --name "large-$clock" --root ${gdbRoot} \
            -- /bin/large > /dev/null
          start=$(rewind events "large-$clock" | grep 'write(1, "start' | awk '{print $1}')
          status=0
          began=$(date +%s%N)
          timeout -k 5 300 rewind gdb "large-$clock" "$start" -- -batch -ex 'watch buffer[0]' \
            -ex continue -ex 'x/2xb $pc-2' -ex continue > large 2>&1 || status=$?
          took=$(( ($(date +%s%N) - began) / 1000000 ))
          cat large
          echo "the large read's session took $took ms"
          test "$status" != $timeout_term
          test "$status" != $timeout_kill
          test "$(grep -c 'hit Hardware watchpoint 1: buffer\[0\]' large)" = 1
          grep -q "New value = 120 'x'" large
          grep -q '^0x[0-9a-f]* <[^>]*>:[[:space:]]*0x0f[[:space:]]*0x05$' large
          test "$took" -lt 5000

          # The kernel makes each record with an out, in a step that began
          # in user space and entered the kernel by a syscall instruction,
          # which clears the trap flag single steps run on. A reverse-stepi
          # from the step of the write's record, then of the exit_group's,
          # stops at that out, in the CPU's thread, and not at the syscall
          # instruction or past the end of the run. A session that hangs
          # is cut short, and fails.
          rewind run -q --clock "$clock" --name "last-$clock" --root ${gdbRoot} \
            -- /bin/last > /dev/null
          rewind events "last-$clock" > events
          for record in 'write(1, "last' 'exit_group(last)'; do
            step=$(grep -F "$record" events | awk '{print $1}')
            status=0
            timeout -k 5 300 rewind gdb "last-$clock" "$step" -- -batch -ex reverse-stepi \
              -ex 'thread 1' -ex 'x/i $pc' > last 2>&1 || status=$?
            cat last
            test "$status" != $timeout_term
            test "$status" != $timeout_kill
            test "$(grep -c 'fatal error\|Remote connection closed' last)" = 0
            grep -q '^=> 0xffffffff[0-9a-f]* <rewind_emit+[0-9]*>:[[:space:]]*out ' last
          done

          # A stepi over the out that made the write's record lands where
          # that step starts, and a reverse-stepi from there goes back to
          # the out: the stop is the step's start, with no step after it.
          step=$(grep -F 'write(1, "last' events | awk '{print $1}')
          status=0
          timeout -k 5 300 rewind gdb "last-$clock" "$step" -- -batch -ex 'thread 1' \
            -ex reverse-stepi -ex 'print $pc' -ex stepi -ex 'print $pc' -ex reverse-stepi \
            -ex 'print $pc' > over 2>&1 || status=$?
          cat over
          test "$status" != $timeout_term
          test "$status" != $timeout_kill
          grep '^\$[123] = ' over | sed 's/^\$[0-9]* = //' > pcs
          test "$(wc -l < pcs)" = 3
          test "$(sed -n 1p pcs)" = "$(sed -n 3p pcs)"
          test "$(sed -n 1p pcs)" != "$(sed -n 2p pcs)"

          # gdb steps the process's thread from write()'s syscall
          # instruction through the kernel until it returns. Until then the
          # thread shows the user pc and sp the call entered with, also in
          # the kernel's entry before it has saved them in pt_regs and in
          # its exit after it has begun to give them back: a stale pc or sp
          # ends the steps early, away from the instruction after the
          # syscall.
          syscall=$(gdb -batch -ex 'disassemble __syscall_cp_c' ${gdbRoot}/bin/last \
            | awk '/syscall *$/ {print $1}')
          start=$(grep -F 'execve("/bin/last"' events | awk '{print $1}')
          printf '%s\n' 'break *$syscall' continue delete 'set $usp = $sp' \
            'set $upc = $pc + 2' stepi 'set $i = 1' \
            'while $i < 50000 && $pc == $upc && $sp == $usp' stepi 'set $i = $i + 1' end \
            'printf "steps %d, returned %d\n", $i, $pc > $upc && $pc < $upc + 16' \
            > through.gdb
          timeout 600 rewind gdb "last-$clock" "$start" -- -batch -ex "set \$syscall = $syscall" \
            -x through.gdb > through 2>&1
          cat through
          grep -q '^steps [0-9]*, returned 1$' through
          test "$(sed -n 's/^steps \([0-9]*\),.*/\1/p' through)" -gt 100

          # A page fault enters the kernel with the trap flag cleared too.
          # The crash's worker faults in user space, and the kernel's
          # handler prints the segfault to the console. A reverse-stepi
          # from the step of that line stops at the out that wrote it.
          rewind run -q --clock "$clock" --name "crash-$clock" --root ${gdbRoot} \
            -- /bin/crash > /dev/null || true
          step=$(rewind events "crash-$clock" | grep 'console ".*crash\[[0-9]*\]: segfault' \
            | awk '{print $1}')
          timeout -k 5 300 rewind gdb "crash-$clock" "$step" -- -batch -ex reverse-stepi \
            -ex 'thread 1' -ex 'x/i $pc' > fault 2>&1 || test $? -lt 124
          cat fault
          test "$(grep -c 'fatal error\|Remote connection closed' fault)" = 0
          grep -q '^=> 0xffffffff[0-9a-f]* <rewind_console_write+[0-9]*>:[[:space:]]*out ' fault

          # An invalid opcode and a privileged instruction in user space,
          # #UD then #GP, enter the kernel's exception entry with the trap
          # flag cleared, and each handler records its signal. A
          # reverse-stepi from the step of each signal's record stops at
          # the out that made it, not at the faulting instruction.
          rewind run -q --clock "$clock" --name "ill-$clock" --root ${gdbRoot} \
            -- /bin/ill > /dev/null
          rewind events "ill-$clock" > events
          for signal in SIGILL SIGSEGV; do
            step=$(grep "$signal code=" events | awk '{print $1}')
            timeout -k 5 300 rewind gdb "ill-$clock" "$step" -- -batch -ex reverse-stepi \
              -ex 'thread 1' -ex 'x/i $pc' > ill 2>&1 || test $? -lt 124
            cat ill
            test "$(grep -c 'fatal error\|Remote connection closed' ill)" = 0
            grep -q '^=> 0xffffffff[0-9a-f]* <rewind_probe_signal+[0-9]*>:[[:space:]]*out ' ill
          done

          # The invalid opcode's IDT stub runs with the trap flag cleared,
          # up to the entry the stubs call. Six steps from the ud2 reach
          # that entry; steps back from there visit the stub's
          # instructions, its call first and its first last, then the ud2,
          # each once.
          ud2=$(gdb -batch -ex 'disassemble main' ${gdbRoot}/bin/ill | awk '/ud2/ {print $1}')
          start=$(grep -F 'write(1, "start' events | awk '{print $1}')
          printf '%s\n' "break *$ud2" continue delete stepi stepi stepi stepi stepi stepi \
            'thread 1' 'x/i $pc' > stub.gdb
          for _ in 1 2 3 4 5 6; do
            printf '%s\n' reverse-stepi 'thread 1' 'x/i $pc' >> stub.gdb
          done
          status=0
          timeout -k 5 300 rewind gdb "ill-$clock" "$start" -- -batch -x stub.gdb > stub 2>&1 \
            || status=$?
          cat stub
          test "$status" != $timeout_term
          test "$status" != $timeout_kill
          sed -n 's/^=> 0x[0-9a-f]* <\([^>]*\)>:[[:space:]]*\([a-z0-9]*\).*/\1 \2/p' stub > visited
          cat visited
          test "$(wc -l < visited)" = 7
          test "$(sed -n 1p visited | cut -d' ' -f1)" = error_entry
          test "$(sed -n 2p visited | cut -d' ' -f2)" = call
          test "$(sed -n 6p visited | cut -d' ' -f1)" = asm_exc_invalid_op
          test "$(sed -n 7p visited | cut -d' ' -f2)" = ud2
          sed -n '2,6p' visited | cut -d' ' -f1 | sed 's/^asm_exc_invalid_op+\{0,1\}//' \
            | sed 's/^$/0/' > offsets
          sort -n -r -u offsets | cmp - offsets
          test "$(wc -l < offsets)" = 5

          # Forwards from the ud2 the steps visit the same instructions in
          # the other order: the stub's first to its call, then the entry.
          printf '%s\n' "break *$ud2" continue delete > forward.gdb
          for _ in 1 2 3 4 5 6; do
            printf '%s\n' stepi 'thread 1' 'x/i $pc' >> forward.gdb
          done
          status=0
          timeout -k 5 300 rewind gdb "ill-$clock" "$start" -- -batch -x forward.gdb > forward 2>&1 \
            || status=$?
          cat forward
          test "$status" != $timeout_term
          test "$status" != $timeout_kill
          sed -n 's/^=> 0x[0-9a-f]* <\([^>]*\)>:[[:space:]]*\([a-z0-9]*\).*/\1 \2/p' forward \
            > forwards
          cat forwards
          test "$(wc -l < forwards)" = 6
          test "$(sed -n 1p forwards | cut -d' ' -f1)" = asm_exc_invalid_op
          test "$(sed -n 5p forwards | cut -d' ' -f2)" = call
          test "$(sed -n 6p forwards | cut -d' ' -f1)" = error_entry
          sed -n '1,5p' forwards | cut -d' ' -f1 | sed 's/^asm_exc_invalid_op+\{0,1\}//' \
            | sed 's/^$/0/' > offsets
          sort -n -u offsets | cmp - offsets
          test "$(wc -l < offsets)" = 5

          # Stepped from the ud2 into the kernel, 200 steps through the stub,
          # error_entry and on into C, the process's thread shows the pc,
          # sp and rdi it faulted with: the CPU pushes them on its entry
          # stack, error_entry copies them to the task's, and the stub's
          # call leaves its return address where rdi's word is for a few
          # instructions.
          printf '%s\n' "break *$ud2" continue delete 'set $urdi = $rdi' 'set $upc = $pc' \
            'set $usp = $sp' 'set $i = 0' 'set $wrong = 0' 'while $i < 200' stepi \
            'if $rdi != $urdi || $pc != $upc || $sp != $usp' 'set $wrong = $wrong + 1' end \
            'set $i = $i + 1' end 'printf "stepped %d, wrong %d\n", $i, $wrong' > faulted.gdb
          status=0
          timeout -k 5 300 rewind gdb "ill-$clock" "$start" -- -batch -x faulted.gdb > faulted 2>&1 \
            || status=$?
          cat faulted
          test "$status" != $timeout_term
          test "$status" != $timeout_kill
          grep -q '^stepped 200, wrong 0$' faulted

          # From the first program's first write, a continue stops before
          # the exec, at the start of the step that runs it, where the
          # loop's total still reads, and does not run into the second
          # program, whose function's address has a breakpoint. A second
          # continue is refused, and stays; a reverse-stepi moves back to
          # the out that made the step's exit, in the CPU's thread, and not
          # to the write's syscall instruction in the process's. The
          # session the message names stops in the second program.
          rewind run -q --clock "$clock" --name "exec-$clock" --root ${gdbRoot} \
            -- /bin/before > /dev/null
          first=$(rewind events "exec-$clock" | grep 'write(1, "before' | awk '{print $1}')
          exec=$(rewind events "exec-$clock" | grep 'execve("/bin/after"' | awk '{print $1}')
          bump=$(gdb -batch -ex 'print/x (long) &bump' ${gdbRoot}/bin/after | awk '{print $NF}')
          status=0
          timeout 300 rewind gdb "exec-$clock" "$first" -- -batch -ex "break *$bump" -ex continue \
            -ex 'print work' -ex 'print $pc' -ex continue -ex 'print $pc' -ex reverse-stepi \
            -ex 'print $pc' > exec 2>&1 || status=$?
          cat exec
          test "$status" != $timeout_term
          said="process [0-9]* runs /bin/after at step $exec; this session debugs the program before it"
          test "$(grep -c "$said" exec)" = 2
          test "$(grep -c 'hit Breakpoint 1' exec)" = 0
          test "$(grep -c 'exited normally' exec)" = 0
          grep -q '^\$1 = 4950$' exec
          grep '^\$[234] = ' exec | sed 's/^\$[0-9]* = //' > pcs
          test "$(wc -l < pcs)" = 3
          test "$(sed -n 1p pcs)" = "$(sed -n 2p pcs)"
          test "$(sed -n 2p pcs)" != "$(sed -n 3p pcs)"
          test "$(sed -n 3p pcs | grep -c '^(void (\*)()) 0xffffffff[0-9a-f]* <rewind_emit+[0-9]*>$')" = 1
          test "$(grep -c 'Thread 2 received signal' exec)" = 0

          # A stepi 100 at the stop before the exec is refused once, as the
          # end of the fork's history, which ends gdb's stepping: a SIGTRAP
          # in the stepped thread would read to gdb as a step done, and it
          # would ask 99 times more. The fork stays where it stopped.
          timeout 300 rewind gdb "exec-$clock" "$first" -- -batch -ex continue \
            -ex 'stepi 100' -ex 'print $pc' > stepped 2>&1
          cat stepped
          test "$(grep -c "$said" stepped)" = 2
          test "$(grep -c 'Reached end of recorded history' stepped)" = 1
          test "$(sed -n 's/^\$1 = //p' stepped)" = "$(sed -n 1p pcs)"

          # A kernel breakpoint in the exec's tail, after the kernel moved
          # the process to the new program's page table and before the
          # exec's record, does not stop the fork in the new program: the
          # continue stops before the exec, where the first one did, and
          # the loop's total still reads.
          timeout 300 rewind gdb "exec-$clock" "$first" -- -batch -ex 'break rewind_probe_exec' \
            -ex continue -ex 'print $pc' -ex 'print work' > tail 2>&1 || test $? -lt 124
          cat tail
          test "$(grep -c "$said" tail)" = 1
          test "$(grep -c 'hit Breakpoint 1' tail)" = 0
          test "$(sed -n 's/^\$1 = //p' tail)" = "$(sed -n 1p pcs)"
          grep -q '^\$2 = 4950$' tail
          again=$(sed -n 's/.*; \(rewind gdb [^ ]* [0-9]*\) debugs the new one$/\1/p;T;q' exec)
          test "$again" != ""
          status=0
          timeout 300 $again -- -batch -ex 'break bump' -ex continue > after 2>&1 || status=$?
          cat after
          test "$status" != $timeout_term
          grep -q 'hit Breakpoint 1, bump (.*after.c' after
        done

        # Under exit time, a reverse-stepi from the last write single-steps
        # the whole loop before it, 10 million rounds, and a reverse-continue
        # to a watch on the sum stops at each of its 10 million writes.
        # Ctrl-C two seconds into either stops it in the loop, where gdb can
        # read the pc, step forwards and back again to the same pc. gdb
        # killed in either ends the session at once.
        rewind run -q --clock exits --name slow --root ${gdbRoot} -- /bin/slow > /dev/null
        done=$(rewind events slow | grep 'write(1, "done' | awk '{print $1}')
        interrupt() {
          local signal=$1 out=$2
          shift 2
          rewind gdb slow "$done" -- -batch -ex 'thread 2' "$@" -ex 'echo begin-reverse\n' \
            -ex "$back" -ex 'echo after-reverse\n' -ex 'print $pc' -ex stepi \
            -ex reverse-stepi -ex 'print $pc' > "$out" 2>&1 &
          local parent=$!
          for _ in $(seq 1 600); do grep -q begin-reverse "$out" && break; sleep 0.1; done
          sleep 2
          kill "-$signal" "$(pgrep -P "$parent" -x gdb)"

          # The stop, or the session's end, within 10 s of the signal.
          for _ in $(seq 1 100); do
            if grep -q 'received signal SIGINT' "$out" || ! kill -0 "$parent" 2> /dev/null; then
              break
            fi
            sleep 0.1
          done
          grep -q 'received signal SIGINT' "$out" || ! kill -0 "$parent" 2> /dev/null || exit 1
          wait "$parent" || true
          cat "$out"
        }
        for back in reverse-stepi reverse-continue; do
          interrupt INT "interrupted-$back" -ex 'watch -l sum'
          grep -q 'received signal SIGINT' "interrupted-$back"
          grep -A1 after-reverse "interrupted-$back" > stopped
          grep -q '^\$[12] = .*<main+' stopped
          grep -E '^\$[0-9]+ = ' "interrupted-$back" | tail -2 | awk '{print $5}' | sort -u > pcs
          test "$(wc -l < pcs)" = 1
          interrupt KILL "killed-$back" -ex 'watch -l sum'
          grep -q 'gdb closed the connection while the fork ran backwards' "killed-$back"
        done

        # A reverse-continue to the watch on the sum, stopped by Ctrl-C six
        # seconds in, leaves the fork past the writes it went through. The
        # next reverse-stepi or reverse-continue replays those writes; Ctrl-C
        # half a second into it stops it within a second, in the loop, where
        # gdb reads the sum and steps forwards and back to the same pc. gdb
        # killed in it ends the session within a second.
        again() {
          local second=$1 signal=$2 out=$3
          rewind gdb slow "$done" -- -batch -ex 'thread 2' -ex 'watch -l sum' \
            -ex 'echo begin-reverse\n' -ex reverse-continue -ex 'echo begin-again\n' \
            -ex "$second" -ex 'echo after-again\n' -ex 'print sum' -ex 'print $pc' \
            -ex stepi -ex reverse-stepi -ex 'print $pc' > "$out" 2>&1 &
          local parent=$!
          local gdb
          for _ in $(seq 1 600); do grep -q begin-reverse "$out" && break; sleep 0.1; done
          gdb=$(pgrep -P "$parent" -x gdb)
          sleep 6
          kill -INT "$gdb"
          for _ in $(seq 1 100); do grep -q begin-again "$out" && break; sleep 0.1; done
          grep -q begin-again "$out"
          sleep 0.5
          kill "-$signal" "$gdb"

          # The second stop, or the session's end, within 1 s of the signal.
          for _ in $(seq 1 10); do
            if [ "$(grep -c 'received signal SIGINT' "$out")" = 2 ] \
              || ! kill -0 "$parent" 2> /dev/null; then
              break
            fi
            sleep 0.1
          done
          [ "$(grep -c 'received signal SIGINT' "$out")" = 2 ] || ! kill -0 "$parent" 2> /dev/null || exit 1
          wait "$parent" || true
          cat "$out"
        }
        for second in reverse-stepi reverse-continue; do
          again "$second" INT "again-$second"
          test "$(grep -c 'received signal SIGINT' "again-$second")" = 2
          grep -A2 after-again "again-$second" > stopped
          grep -q '^\$[0-9]* = [0-9]' stopped
          grep -q '^\$[0-9]* = .*<main+' stopped
          grep -E '^\$[0-9]+ = \(void' "again-$second" | tail -2 | awk '{print $5}' | sort -u > pcs
          test "$(wc -l < pcs)" = 1
        done
        again reverse-stepi KILL killed-again
        grep -q 'gdb closed the connection while the fork ran backwards' killed-again

        # A continue from the spin's write runs its loop, with no exit, to
        # the exec, then goes back to the step's start, where it stops. A
        # Ctrl-C half a second in reaches the fork as it goes back, and the
        # stop before the exec answers it: the next continue is refused
        # there, not stopped at once by the Ctrl-C, and a reverse-stepi
        # moves. A session that hangs is cut short, and fails.
        rewind run -q --clock exits --name spin --root ${gdbRoot} -- /bin/spin > /dev/null
        spin=$(rewind events spin | grep 'write(1, "spin' | awk '{print $1}')
        exec=$(rewind events spin | grep 'execve("/bin/after"' | awk '{print $1}')
        rewind gdb spin "$spin" -- -batch -ex 'echo begin-forward\n' -ex continue \
          -ex 'print $pc' -ex continue -ex 'print $pc' -ex reverse-stepi -ex 'print $pc' \
          > spun 2>&1 &
        parent=$!
        for _ in $(seq 1 600); do grep -q begin-forward spun && break; sleep 0.1; done
        grep -q begin-forward spun
        sleep 0.5
        kill -INT "$(pgrep -P "$parent" -x gdb)"
        timeout 120 tail --pid="$parent" -f /dev/null
        wait "$parent" || true
        cat spun
        said="process [0-9]* runs /bin/after at step $exec; this session debugs the program before it"
        test "$(grep -c "$said" spun)" = 2
        test "$(grep -c 'received signal SIGINT' spun)" = 0
        test "$(grep -c 'fatal error\|Remote connection closed' spun)" = 0
        grep '^\$[123] = ' spun | sed 's/^\$[0-9]* = //' > pcs
        test "$(wc -l < pcs)" = 3
        test "$(sed -n 1p pcs)" = "$(sed -n 2p pcs)"
        test "$(sed -n 2p pcs)" != "$(sed -n 3p pcs)"
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
          gdb
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
        grep -q 'hit Breakpoint 1, 0x00007[0-9a-f]* in clock_gettime ()' gdb
        grep -q '^#1  0x0000000000[0-9a-f]* in clock_gettime ()' gdb
        touch $out
      '';

  # checks.where: `rewind where` names the program's own line at a step.
  # At the worker's write, the innermost frames are musl's printf
  # machinery, which has no line table; the answer is the worker's printf
  # in pool.c, with its source from the VM and the line marked, and two
  # frames that called it. --json lists the same frame as the chosen one,
  # and carries pool.c whole with the write on that line. Both list the
  # worker's argument and local. The JSON names stdout, on the line after
  # the write, as in memory and small enough to watch, and leaves the
  # words of the write's comment out. The main thread, off the CPU then, waits in
  # pool_run's join, called from main, whose struct reads as main set it
  # and is in memory. Boots the VM, so it needs /dev/kvm.
  where =
    pkgs.runCommand "rewind-where"
      {
        nativeBuildInputs = [
          rewind
          gdb
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
        grep -q '^arguments:$' where
        grep -q '^  arg = ' where
        grep -q '^locals:$' where
        grep -q '^  p = ' where

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
        jq -e '.frames[.chosen] | (.arguments | map(.name)) == ["arg"]
          and (.locals | map(.name)) == ["p"]' where.json

        # The line after the write names stdout, a pointer in memory,
        # small enough to watch; the words of the write's comment are no
        # expression.
        jq -e --argjson line "$(line 'the write')" '.frames[.chosen].expressions
          | (map(select(.line == $line + 1 and .text == "stdout"))[0].location
              | .kind == "memory" and .size == 8 and .class == "pointer")
            and (map(select(.line == $line)) | all(.text != "write" and .text != "the"))' where.json

        rewind where pool "$step" --tid "$pid" > main
        cat main
        grep -q "^#[1-9][0-9]* pool_run (pool.c:$(line 'the wait'))$" main
        grep -q '^called from #[0-9]* main (main.c:' main
        grep -q '^  t = ' main
        rewind where pool "$step" --tid "$pid" --json > main.json
        jq -e '.frames[] | select(.function == "main") | .locals[]
          | select(.name == "p") | .value == "{job = 1}"' main.json
        jq -e '.frames[] | select(.function == "main") | .locals[]
          | select(.name == "p") | .location.kind == "memory"' main.json

        # Removing the run takes its source cache with it.
        id=$(basename "$(dirname "$(grep -l '"name": "pool"' $REWIND_HOME/runs/*/manifest.json)")")
        test -d $REWIND_HOME/cache/sources/$id
        rewind remove pool
        test ! -e $REWIND_HOME/cache/sources/$id
        touch $out
      '';

  # checks.where-idle: `rewind where` answers for a program whose 200
  # source files the VM alone has. The first look at a run asks the VM for
  # all of them, and the list of their paths, over 4 KiB, goes in as
  # typing. When the typing went in before the inspection read any, which
  # it did whenever the fork's step fell a few steps short of a multiple of
  # 4096, where the monitor checks for typing, the VM's console dropped
  # what did not fit, and the inspection waited for the rest forever. Each
  # step from 4060 to 4094 is looked at first, its sources unread, with a
  # timeout that turns a hang into a failure. Boots the VM, so it needs
  # /dev/kvm.
  where-idle =
    pkgs.runCommand "rewind-where-idle"
      {
        nativeBuildInputs = [
          rewind
          gdb
          pkgs.jq
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        export REWIND_DEBUGINFOD=/nonexistent

        rewind run -q --name idle --root ${idleRoot} -- /bin/idle
        test "$(rewind ls --json | jq -s '.[0].steps')" -gt 4096
        pid=$(rewind threads idle --json | jq -s -r 'map(select(.name == "idle"))[0].pid')
        wait=$(grep -n 'the wait' ${idleRoot}/src/main.c | cut -d: -f1)

        for step in $(seq 4060 2 4094); do
          rm -rf $REWIND_HOME/cache/sources
          timeout 120 rewind where idle "$step" --pid "$pid" --tid "$pid" > main \
            || { echo "rewind where at step $step failed or hung"; exit 1; }
          grep -q "^#[0-9]* main (main.c:$wait)$" main
        done
        cat main
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
          gdb
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
        if rewind check --where -j 4 --clock exits --root ${raceRoot} -- /bin/race > found; then exit 1; fi
        cat found
        grep -q ' decides it: ' found
        # A reschedule decides it only where a thread is between its read
        # and its store, in the write: add, at that line, not the C
        # library's inline write it calls.
        exit_line=$(grep -n 'the exit' ${raceRoot}/src/race.c | cut -d: -f1)
        thread='^ +[0-9]+ +[0-9]+/[0-9]+ +'
        grep -qE "$thread"'add \(race\.c:'"$exit_line"'\), on the CPU at the deciding step$' found
        grep -qE "$thread"'[a-z_]+ \(race\.c:[0-9]+\), at the first event that differs$' found

        if rewind check --where --json -j 4 --clock exits --root ${raceRoot} -- /bin/race > found.json; then exit 1; fi
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
        if rewind check --run race --schedule-from "$from" > found; then exit 1; fi
        cat found
        grep -q ' decides it: ' found

        # Options that make another run than the one named are refused.
        if rewind check --run race --mem 2048 2> refused; then exit 1; fi
        grep -q -- '--mem' refused
        touch $out
      '';

  # checks.check-dirents: `rewind check --dirents` runs a job again under
  # other directory order keys. A job that passes only when a tmpfs
  # directory lists its newest file first passes under the stock order
  # and fails under some key: check names that key on its line, reports
  # it against schedule 0 without narrowing, and the two runs it reports
  # keep their keyframes. With --all it counts the key runs, and its JSON
  # names each run's key; with --seeds only the first seed runs under the
  # keys. A job that lists no directory ends the same under every key.
  # --dirents is refused beside --run and beside a --kernel-args that
  # sets the key. Boots the VM, so it needs /dev/kvm.
  check-dirents =
    pkgs.runCommand "rewind-check-dirents"
      {
        nativeBuildInputs = [
          rewind
          pkgs.jq
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        # Five files made in order on /dev/shm, a tmpfs, which lists the
        # newest first. busybox find lists a directory unsorted, and the
        # job passes when the first file it lists is e, the newest.
        newest='mkdir /dev/shm/d && for f in a b c d e; do echo > /dev/shm/d/$f; done
        set -- $(busybox find /dev/shm/d -mindepth 1); test "$1" = /dev/shm/d/e'
        check() { rewind check -j 4 --clock exits --schedules 0 "$@" --root ${busyboxRoot} -- sh -c "$newest"; }

        # The search: the first key that ends differently, reported
        # against the stock order with no step to narrow to.
        if check --dirents 4 > found; then exit 1; fi
        cat found
        grep -q '^dirents  0 schedule   0: exited:0 ' found
        grep -qE '^dirents  [1-4] schedule   0: exited:1 ' found
        grep -qE '^dirents [1-4] decides it: under schedule 0, dirents 0 passes and dirents [1-4] fails$' found
        grep -qE '^passing: run [0-9a-f]+, dirents 0, schedule 0$' found
        grep -qE '^failing: run [0-9a-f]+, dirents [1-4], schedule 0$' found
        test "$(grep -c 'finding the step' found)" = 0

        # The two reported runs keep keyframes, the key's run taking them
        # once it is reported, and replay as recorded.
        for side in passing failing; do
          id=$(sed -n "s/^$side: run \([0-9a-f]*\),.*/\1/p" found)
          ls $REWIND_HOME/runs/$id/keyframes > keyframes
          grep -q '\.kf$' keyframes
          rewind replay $id | grep '^identical'
        done

        # Every key, counted, and named in the JSON.
        if check --dirents 4 --all --no-narrow > all; then exit 1; fi
        cat all
        test "$(grep -cE '^dirents  [1-4] schedule   0: ' all)" = 4
        grep -qE '^[1-4] of 4 other runs ended differently$' all
        if check --dirents 4 --all --no-narrow --json > all.json; then exit 1; fi
        jq -e '.dirents == 4 and .tried == 4 and [.schedules[].dirents] == [0, 1, 2, 3, 4]' all.json
        jq -e '.differing > 0 and ([.schedules[] | select(.differs)] | length) == .differing' all.json

        # With two seeds, only the first runs under the keys.
        if check --seeds 2 --dirents 2 --all --no-narrow > seeded; then exit 1; fi
        cat seeded
        grep -q ': 4 runs$' seeded
        test "$(grep -cE '^seed  0 dirents  [12] schedule   0: ' seeded)" = 2
        grep -q '^seed  1 dirents  0 schedule   0: ' seeded

        # A job that lists no directory ends the same under every key.
        rewind check -j 4 --clock exits --schedules 0 --dirents 4 --all \
          --root ${busyboxRoot} -- sh -c 'echo same' > same
        cat same
        grep -q '^0 of 4 other runs ended differently$' same
        grep -q '^same result under all 5 runs$' same

        # A recorded run's key stands, as does one --kernel-args gives.
        if rewind check --run abc --dirents 2 2> refused; then exit 1; fi
        grep -q -- '--dirents' refused
        if check --dirents 2 --kernel-args rewind.dirents=1 2> refused; then exit 1; fi
        cat refused
        grep -q -- '--kernel-args sets rewind.dirents' refused
        touch $out
      '';

  # checks.check-point: the SDK's rewind_point is a schedule point. point's
  # threads race only in computation, so with no points every schedule
  # holds; with a point between each load and store, check finds a
  # schedule that loses an add and narrows it to a step. A point is one
  # exit and leaves no mark in the trace. Boots the VM, so it needs
  # /dev/kvm.
  check-point =
    pkgs.runCommand "rewind-check-point"
      {
        nativeBuildInputs = [
          rewind
          pkgs.jq
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        check() { rewind check -j 4 --clock exits --property exit-zero --root ${pointRoot} "$@"; }

        # Without points the threads never switch inside an add.
        check --schedules 32 -- /bin/point > plain
        cat plain
        grep -q '^every property held under all 33 schedules$' plain

        # With them, a schedule loses an add.
        if check -- /bin/point points > found; then exit 1; fi
        cat found
        grep -qE '^schedule +[0-9]+ violates exit-zero; finding the step' found
        grep -q ' decides it: .* makes the run violate exit-zero$' found

        # Each point is an exit, and none is a mark.
        marks() { rewind events --json "$1" | jq -s '[.[] | select(.kind.type == "mark")] | length'; }
        rewind run --clock exits --root ${pointRoot} -- /bin/point
        plain_id=$(rewind ls --json -n 1 | jq -r .id)
        rewind run --clock exits --root ${pointRoot} -- /bin/point points || true
        points_id=$(rewind ls --json -n 1 | jq -r .id)
        plain_steps=$(rewind ls --json -n 2 | jq -rs '.[1].steps')
        points_steps=$(rewind ls --json -n 1 | jq -r .steps)
        echo "steps: $plain_steps plain, $points_steps with points"
        test "$points_steps" -ge $((plain_steps + 20))
        plain_marks=$(marks "$plain_id")
        points_marks=$(marks "$points_id")
        echo "marks: $plain_marks plain, $points_marks with points"
        test "$plain_marks" -gt 0
        test "$plain_marks" = "$points_marks"
        touch $out
      '';

  # checks.check-property: `rewind check --property` judges each schedule
  # by what a correct run is instead of by schedule 0's outcome. A script
  # that reads race's total from its log finds a schedule that loses an
  # add and narrows it, naming the property and the reason; exit-zero
  # counts the same schedules; a script that reads a file at the job's
  # exit with `rewind cat` sees the root's own file; a property schedule
  # 0 violates too is reported at its step; a script that fails stops
  # the search; --seeds finds a job decided by the kernel's randomness
  # alone; a shell job's Rewind SDK marks are judged without being asked
  # for; and junit= reads the JUnit report a job wrote, from a keyframe
  # each run took where its job exited. Boots the VM, so it needs
  # /dev/kvm.
  check-property =
    pkgs.runCommand "rewind-check-property"
      {
        nativeBuildInputs = [
          rewind
          pkgs.jq
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        check() { rewind check -j 4 --clock exits "$@" --root ${raceRoot} -- /bin/race; }

        # race's total, from its log, against the adds its threads made.
        cat > total.sh <<'EOF'
        #!/bin/sh
        total=$(rewind log "$REWIND_RUN" | sed -n 's/^total //p')
        [ "$total" = 20 ] && exit 0
        echo "total $total, expected 20"
        exit 1
        EOF
        chmod +x total.sh

        # The search: a schedule that violates the property, narrowed.
        if check --property script=./total.sh > found; then exit 1; fi
        cat found
        grep -q '^schedule   0: .*  holds$' found
        grep -qE '^schedule +[0-9]+ violates total; finding the step' found
        grep -q ' decides it: .* makes the run violate total$' found
        grep -qE '^failing run violates total: total [0-9]+, expected 20$' found
        if check --property script=./total.sh --json > found.json; then exit 1; fi
        jq -e '.properties == ["script=./total.sh"]' found.json
        jq -e '.narrowed.violations | length == 1 and .[0].property == "total"' found.json

        # exit-zero fails the same schedules race's own exit status does.
        check --property exit-zero --schedules 8 --all --no-narrow --json > swept.json || true
        jq -e '([.schedules[] | select(.differs)] | length) == .differing' swept.json
        jq -e 'all(.schedules[]; (.violations | length > 0) == (.ending != "exited:0"))' swept.json

        # A file read at the job's exit is the root's own.
        cat > binary.sh <<'EOF'
        #!/bin/sh
        rewind cat "$REWIND_RUN" "$REWIND_END" /bin/race | cmp -s - ${raceRoot}/bin/race
        EOF
        chmod +x binary.sh
        check --property script=./binary.sh --schedules 2 --all > read
        cat read
        test "$(grep -c '  holds$' read)" = 3
        grep -q '^every property held under all 3 schedules$' read

        # A property schedule 0 violates as well: the bug is at its step.
        printf '#!/bin/sh\necho "step 7: always wrong"\nexit 1\n' > wrong.sh
        chmod +x wrong.sh
        if check --property script=./wrong.sh --schedules 2 > wrong; then exit 1; fi
        cat wrong
        grep -q '^  wrong: step 7: always wrong$' wrong
        grep -q '^every schedule violated a property, as schedule 0 did$' wrong
        grep -qE '^open it at the step: rewind open [0-9a-f]+ 7$' wrong

        # A script that fails stops the search and says why.
        printf '#!/bin/sh\necho broken >&2\nexit 3\n' > broken.sh
        chmod +x broken.sh
        if check --property script=./broken.sh --schedules 2 2> stderr; then exit 1; fi
        cat stderr
        grep -q 'property script ./broken.sh failed on run .*: broken' stderr

        # The coin jobs below run at one fixed epoch. The guest kernel mixes
        # its boot clock into its randomness, and the default epoch is the
        # start of the current day, so without a fixed one the six seeds'
        # coins land anew each day, and all six land the same way about one
        # day in 64.
        epoch=1790985600

        # Seeds: a job that fails when the first byte it reads from the
        # kernel's randomness is odd. Some seed of six decides it with no
        # schedule perturbed, and every run says its seed.
        coin='b=$(od -An -tu1 -N1 /dev/urandom); test $((b % 2)) = 0'
        if rewind check -j 4 --clock exits --epoch $epoch --seeds 6 --schedules 1 --root ${busyboxRoot} -- sh -c "$coin" > seeds; then exit 1; fi
        cat seeds
        grep -qE '^seed  0 schedule   0: ' seeds
        grep -qE '^seed [0-9]+ decides it: under schedule 0, seed [0-9]+ passes and seed [0-9]+ fails$' seeds
        if rewind check -j 4 --clock exits --epoch $epoch --seeds 6 --schedules 1 --all --no-narrow --json \
          --root ${busyboxRoot} -- sh -c "$coin" > seeds.json; then exit 1; fi
        jq -e '.seeds == 6 and ([.schedules[].seed] | unique) == [0, 1, 2, 3, 4, 5]' seeds.json
        jq -e '.differing > 0 and ([.schedules[] | select(.differs)] | length) == .differing' seeds.json
        # A recorded run tried under other seeds: each a fork of it at the
        # job's start, its RNG reseeded there. The coin may land either
        # way under the run's own seed, so its job exits 0 or 1.
        status=0
        rewind run -q --clock exits --epoch $epoch --name coin --root ${busyboxRoot} -- sh -c "$coin" || status=$?
        [ "$status" -le 1 ]
        if rewind check --run coin --seeds 6 --schedules 0 > reseeded; then exit 1; fi
        cat reseeded
        grep -qE '^seed [0-9]+ decides it: under schedule 0, seed [0-9]+ passes and seed [0-9]+ fails$' reseeded
        grep -qE '^failing: run [0-9a-f]+, seed [0-9]+ from step [0-9]+, schedule 0$|^passing: run [0-9a-f]+, seed [0-9]+ from step [0-9]+, schedule 0$' reseeded

        # rewind fork --seed: the fork is its parent until the step, and
        # replays the same. fork exits with its job's status, and the coin
        # under seed 9 lands either way depending on the host's CPU.
        start=$(rewind events coin --json | jq -r 'select(.kind.type == "mark" and .kind.text == "rewind-start") | .step')
        status=0
        rewind fork -q coin "$start" --seed 9 --json > forked.json || status=$?
        [ "$status" -le 1 ]
        jq -e '.name | test("seed 9")' forked.json
        rewind replay "$(jq -r .id forked.json)" > replayed
        grep -q '^identical' replayed

        # The Rewind SDK from a shell: a job that asserts its random byte
        # is even and marks a test, and exits 0 either way. check judges
        # the marks by itself, finds the seed whose assertion fails, names
        # the sometimes no run can make true, a byte being at most 255, and
        # counts the runs that made the other true.
        cat > coin.sh <<'EOF'
        sdk() { printf 'rewind-sdk 1 %s\n' "$*" > /dev/rewind; }
        b=$(od -An -tu1 -N1 /dev/urandom)
        b=$((b + 0))
        sdk test-begin coin flip
        if [ $((b % 2)) = 0 ]; then
          sdk always pass even
          sdk test-end pass coin flip
        else
          sdk always fail even byte $b is odd
          sdk test-end fail coin flip
        fi
        if [ $b -gt 255 ]; then sdk sometimes hit huge-byte; else sdk sometimes miss huge-byte; fi
        if [ $((b % 2)) = 0 ]; then sdk sometimes hit even-byte; else sdk sometimes miss even-byte; fi
        exit 0
        EOF
        if rewind check -j 4 --clock exits --epoch $epoch --seeds 6 --schedules 0 --root ${busyboxRoot} -- sh -c "$(cat coin.sh)" > sdk; then exit 1; fi
        cat sdk
        grep -q '^rewind: the job asserts with the Rewind SDK, so each run is judged by property sdk too$' sdk
        grep -qE '^failing run violates sdk: step [0-9]+: always "even" failed: byte [0-9]+ is odd$' sdk
        grep -q '^never true in any run: sometimes "huge-byte"$' sdk
        grep -qE '^true in [1-9][0-9]* of [0-9]+ runs that checked it: sometimes "even-byte"$' sdk
        if rewind check -j 4 --clock exits --epoch $epoch --seeds 6 --schedules 0 --all --no-narrow --json \
          --root ${busyboxRoot} -- sh -c "$(cat coin.sh)" > sdk.json; then exit 1; fi
        jq -e '.properties == ["same-outcome", "sdk"] and .sometimes_never == ["huge-byte"]' sdk.json
        jq -e '.sometimes_true | length == 1 and .[0].id == "even-byte" and .[0].runs == 6 and .[0].true >= 1' sdk.json
        jq -e '[.schedules[].violations[] | select(.property == "sdk") | .why]
          | all(test("^(always \"even\" failed: byte [0-9]+ is odd|test coin flip passed in the unperturbed run and failed here)$"))' sdk.json

        # JUnit reports: a job that writes one with two tests, coin.flip
        # failing when its random byte is even and coin.flop when it is
        # odd, so one of them passes in the base run whichever way seed
        # 0's byte lands on this CPU. check reads the report each run
        # wrote and names the test that passed under seed 0 and failed.
        cat > report.sh <<'EOF'
        b=$(od -An -tu1 -N1 /dev/urandom)
        flip=""
        flop=""
        if [ $((b % 2)) = 0 ]; then flip="<failure message=\"even\"/>"; else flop="<failure message=\"odd\"/>"; fi
        mkdir -p /run/reports
        printf '<testsuite><testcase classname="coin" name="flip">%s</testcase><testcase classname="coin" name="flop">%s</testcase></testsuite>\n' "$flip" "$flop" > /run/reports/TEST-coin.xml
        EOF
        # The search has a home of its own, holding only its runs.
        junit_home=$TMPDIR/junit-home
        if REWIND_HOME=$junit_home rewind check -j 4 --clock exits --epoch $epoch --seeds 6 --schedules 0 \
          --property 'junit=/run/reports/*.xml' --root ${busyboxRoot} -- sh -c "$(cat report.sh)" > junit; then exit 1; fi
        cat junit
        grep -qE '^failing run violates junit: step [0-9]+: test coin\.(flip|flop) passed in the unperturbed run and failed here$' junit

        # Each other seed's run took a keyframe where its job exited, where
        # check read its report, and replays as recorded all the same. The
        # failing run check reports has keyframes before then besides.
        exit_step() { REWIND_HOME=$junit_home rewind events $1 | awk '/mark "rewind-exit / { print $1 }'; }
        base=$(grep -m1 -oE 'run [0-9a-f]{16}' junit | cut -d' ' -f2)
        for dir in $junit_home/runs/*; do
          id=$(basename $dir)
          REWIND_HOME=$junit_home rewind replay $id | grep '^identical'
          if [ $id = $base ]; then continue; fi
          test -e $dir/keyframes/$(printf %016d $(exit_step $id)).kf
        done
        failing=$(sed -n 's/^failing: run \([0-9a-f]*\),.*/\1/p' junit)
        first=$(ls $junit_home/runs/$failing/keyframes | awk -F. '/\.kf$/ && !seen++ { print $1 + 0 }')
        test "$first" -lt "$(exit_step $failing)"

        # A junit pattern that matches no report schedule 0 wrote stops
        # the search, where it would hold on every run: * stays within one
        # directory, so */reports/*.xml misses /run/reports.
        if rewind check -j 4 --clock exits --schedules 0 --property 'junit=*/reports/*.xml' \
          --root ${busyboxRoot} -- sh -c "$(cat report.sh)" 2> unmatched; then exit 1; fi
        cat unmatched
        grep -q 'junit=\*/reports/\*.xml matched no JUnit report schedule 0 wrote' unmatched

        # A misspelled property is refused before anything boots.
        if rewind check --property exit-0 --root ${raceRoot} -- /bin/race 2> refused; then exit 1; fi
        grep -q 'no property exit-0' refused
        touch $out
      '';

  # checks.trace-properties: the properties `rewind check --property`
  # decides from the trace alone, each on a shell job in the busybox root
  # that breaks it, and on one that does not: a child killed by SIGSEGV
  # while the job exits 0, a sleep left running, two sibling processes
  # appending to one log at once, a config rewritten in place, a reader
  # opening a file another process is writing, and a write outside the
  # job's directory. A properties file lists them with a comment and a
  # script beside it. Boots the VM, so it needs /dev/kvm.
  trace-properties =
    pkgs.runCommand "rewind-trace-properties"
      {
        nativeBuildInputs = [
          rewind
          pkgs.jq
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        # One unperturbed and one perturbed schedule of a shell job, judged
        # by the property; exits 1 when schedule 0 violates it. The root
        # has no /run until the job makes it, and an open that fails
        # writes nothing.
        prop() {
          rewind check -j 2 --clock exits --schedules 1 --all --no-narrow \
            --property "$1" --root ${busyboxRoot} -- sh -c "mkdir -p /run; $2" > out
          status=$?
          cat out
          return $status
        }

        if prop no-crash 'sh -c "kill -SEGV \$\$"; exit 0'; then exit 1; fi
        grep -qE '^  no-crash: step [0-9]+: process [0-9]+ \(sh\) died of SIGSEGV$' out
        prop no-crash 'sh -c "exit 3"; exit 0'

        if prop no-orphans 'sleep 100 & sleep 1; exit 0'; then exit 1; fi
        grep -qE '^  no-orphans: step [0-9]+: process [0-9]+ \(sleep\), started at step [0-9]+, was still running when the job.s main process exited at step [0-9]+$' out
        prop no-orphans 'sleep 1 & wait'

        if prop 'single-writer=*.log' '(exec 3>> /run/w.log; echo a >&3; sleep 2) & (sleep 1; echo b >> /run/w.log) & wait'; then exit 1; fi
        grep -qE '^  single-writer: step [0-9]+: process [0-9]+ \(sh\) opened /run/w.log for writing while process [0-9]+ \(sh\), which opened it at step [0-9]+, had not closed it$' out
        prop 'single-writer=*.log' '(echo a >> /run/w.log) & wait; (echo b >> /run/w.log) & wait'

        if prop 'atomic-replace=*.conf' 'echo a > /run/app.conf; echo b > /run/app.conf'; then exit 1; fi
        grep -qE '^  atomic-replace: step [0-9]+: process [0-9]+ \(sh\) truncated /run/app.conf in place, which the run wrote at step [0-9]+' out
        prop 'atomic-replace=*.conf' 'echo a > /run/app.conf; echo b > /run/new.conf; cat /run/new.conf > /dev/null'

        if prop writes-within=/run 'echo a > /run/ok; echo b > /stray'; then exit 1; fi
        grep -qE '^  writes-within: step [0-9]+: process [0-9]+ \(sh\) opened /stray for writing, outside /run$' out
        prop writes-within=/run 'echo a > /run/ok; echo b > /dev/null'

        # A reader opening a file another process holds open for writing,
        # recorded because the kernel counts a writer, and the same read
        # once the writer has closed it, never recorded. Two writers one
        # after the other, the first closing before the second opens, are
        # not two at once.
        if prop 'no-read-while-written=*.conf' '(exec 3> /run/app.conf; echo a >&3; sleep 2) & sleep 1; cat /run/app.conf; wait'; then exit 1; fi
        grep -qE '^  no-read-while-written: step [0-9]+: process [0-9]+ \(cat\) opened /run/app.conf for reading while process [0-9]+ \(sh\), which opened it for writing at step [0-9]+, had not closed it$' out
        prop 'no-read-while-written=*.conf' 'echo a > /run/app.conf; cat /run/app.conf'
        prop 'single-writer=*.log' '(echo a >> /run/w.log; sleep 2) & (sleep 1; echo b >> /run/w.log) & wait'

        # A properties file: comments skipped, a script taken from the
        # file's directory, and the verdicts of both in the JSON.
        mkdir props
        printf '#!/bin/sh\nexit 0\n' > props/fine.sh
        chmod +x props/fine.sh
        printf '# what a correct run is\nno-orphans\n\nscript=fine.sh\n' > props/rewind.properties
        if rewind check -j 2 --clock exits --schedules 1 --all --no-narrow --json \
          --properties props/rewind.properties --root ${busyboxRoot} -- sh -c 'sleep 100 & exit 0' > file.json; then exit 1; fi
        jq -e '.properties == ["no-orphans", "script=props/fine.sh"]' file.json
        jq -e '.schedules | all(.violations | map(.property) == ["no-orphans"])' file.json
        touch $out
      '';

  # checks.threads: `rewind threads` says what held the CPU at each step
  # of a window, one slice a line: the thread of every event in the window
  # held it at the event's step, both of race's workers show up, the
  # slices cover the window without a gap, a window past the run's end is
  # cut there, and one from boot starts at the kernel's first recorded
  # context switch. Boots the VM, so it needs /dev/kvm.
  threads =
    pkgs.runCommand "rewind-threads"
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
        last=$(rewind ls --json -n 1 | jq .steps)
        from=$(rewind events race | grep 'execve("/bin/race"' | awk '{print $1}')
        rewind threads race --from "$from" --to "$((last + 50))" --json > slices
        rewind events race --from "$from" --json > events
        cat slices

        jq -se --argjson from "$from" --argjson last "$last" '
          .[0].from == $from and .[-1].to == $last
          and ([range(1; length) as $i | .[$i].from == .[$i - 1].to + 1] | all)
        ' slices
        jq -se '[.[] | select(.on == "thread") | .tid] | unique | length >= 3' slices

        # A window from boot starts at the first recorded switch, before
        # which no record says who ran.
        rewind threads race --from 0 --to "$from" --json > booting
        jq -se --argjson from "$from" '
          length > 0 and .[0].from > 0 and .[-1].to == $from
        ' booting
        jq -se --slurpfile events events '
          . as $slices
          | [$events[] | select(.pid != 0) as $e
              | $slices[] | select(.from <= $e.step and $e.step <= .to)
              | .on == "thread" and .pid == $e.pid and .tid == $e.tid]
          | length > 20 and all
        ' slices
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

  # checks.sqpoll: a program with an io_uring whose submissions a kernel
  # thread polls for (IORING_SETUP_SQPOLL) finishes under every schedule.
  # The thread polls in a loop with no exit until its idle time passes,
  # and virtual time moves only at exits, so before each poll made one the
  # VM ran io_sq_thread forever. The program waits past the idle time
  # between its two submissions, so the thread also has to go to sleep and
  # be woken. libuv's io_uring tests set up such rings. Boots the VM, so it
  # needs /dev/kvm.
  sqpoll =
    let
      source = pkgs.writeText "sqpoll.c" ''
        #include <liburing.h>
        #include <stdio.h>
        #include <string.h>
        #include <unistd.h>

        /* How long the polling thread polls with nothing to do before it
           sleeps, in milliseconds, as libuv sets it. */
        #define IDLE_MS 10

        /* Submits a no-op and waits for its completion. */
        static int nop(struct io_uring *ring) {
            struct io_uring_sqe *sqe = io_uring_get_sqe(ring);
            io_uring_prep_nop(sqe);
            if (io_uring_submit(ring) != 1) {
                return -1;
            }
            struct io_uring_cqe *cqe;
            if (io_uring_wait_cqe(ring, &cqe) != 0) {
                return -1;
            }
            io_uring_cqe_seen(ring, cqe);
            return 0;
        }

        int main(void) {
            struct io_uring ring;
            struct io_uring_params params;
            memset(&params, 0, sizeof(params));
            params.flags = IORING_SETUP_SQPOLL;
            params.sq_thread_idle = IDLE_MS;
            int err = io_uring_queue_init_params(8, &ring, &params);
            if (err != 0) {
                printf("io_uring_queue_init_params: %s\n", strerror(-err));
                return 1;
            }

            /* One submission while the thread polls, and one after it
               has gone idle and has to be woken. */
            if (nop(&ring) != 0) {
                puts("first nop failed");
                return 1;
            }
            usleep(5 * IDLE_MS * 1000);
            if (nop(&ring) != 0) {
                puts("second nop failed");
                return 1;
            }

            io_uring_queue_exit(&ring);
            puts("done");
            return 0;
        }
      '';
      root = pkgs.pkgsStatic.runCommandCC "rewind-sqpoll-root" { } ''
        mkdir -p $out/bin
        $CC -static -O2 -I${pkgs.pkgsStatic.liburing.dev}/include \
          -o $out/bin/sqpoll ${source} ${pkgs.pkgsStatic.liburing}/lib/liburing.a
      '';
    in
    pkgs.runCommand "rewind-sqpoll"
      {
        nativeBuildInputs = [
          rewind
          pkgs.coreutils
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        rewind run --timeout 120 --root ${root} -- /bin/sqpoll | tee out
        grep -qx done out
        timeout 600 rewind check -j 4 --schedules 16 --timeout 120 --root ${root} -- /bin/sqpoll | tee found
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
    test "$(grep -c 'HOME' out)" = 0
    touch $out
  '';

  # checks.sdk-languages: the Rewind SDK's libraries for C, Rust, Go,
  # shell, Python with its pytest plugin, and Java with its JUnit 5
  # extension, each built from sdk/ as a program would use it. Outside
  # Rewind every call does nothing; in a run, each library writes exactly
  # its lines, a failure made twice written once; and `rewind check`
  # judges them by itself. Boots the VM, so it needs /dev/kvm.
  sdk-languages =
    pkgs.runCommand "rewind-sdk-languages"
      {
        nativeBuildInputs = [
          rewind
          pkgs.jq
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind

        # Outside Rewind: no /dev/rewind, nothing written, nothing broken.
        test ! -e /dev/rewind
        ${sdkLanguages.demos} > outside 2>&1
        cat outside

        rewind run -q --clock exits --mem 2048 --name sdk --root ${sdkLanguages.root} -- /bin/sdk-demos
        rewind events sdk --json \
          | jq -r 'select(.kind.type == "mark" and .pid != 1) | .kind.text' \
          | grep '^rewind-sdk ' > marks
        cat marks

        # Each line, written exactly once.
        once() {
          if [ "$(grep -cxF -- "rewind-sdk 1 $1" marks)" != 1 ]; then
            echo "not written exactly once: rewind-sdk 1 $1"
            exit 1
          fi
        }
        for lang in c rust go sh python; do
          once "test-begin $lang demo"
          once "always pass $lang-holds"
          once "always fail $lang-fails $lang said so"
          once "sometimes miss $lang-sometimes"
          once "unreachable $lang-unreachable reached"
          once "test-end fail $lang demo"
        done
        once "test-begin test_demo.py::test_passes"
        once "test-end pass test_demo.py::test_passes"
        once "test-begin test_demo.py::test_fails"
        once "test-end fail test_demo.py::test_fails"
        once "always pass pytest-holds"
        once "always fail pytest-fails pytest said so"
        once "test-begin DemoTest.passes"
        once "test-end pass DemoTest.passes"
        once "test-begin DemoTest.fails"
        once "test-end fail DemoTest.fails"
        once "always pass java-holds"
        once "always fail java-fails java said so"
        once "sometimes miss java-sometimes"
        once "unreachable java-unreachable reached"
        test "$(wc -l < marks)" = 44

        # check judges the marks without being asked: the first failure
        # is C's, and no run made any sometimes true.
        if rewind check -j 2 --clock exits --mem 2048 --schedules 0 --json \
          --root ${sdkLanguages.root} -- /bin/sdk-demos > checked.json; then exit 1; fi
        jq -e '.properties == ["same-outcome", "sdk"]' checked.json
        jq -e '.schedules[0].violations[0].why == "always \"c-fails\" failed: c said so"' checked.json
        jq -e '.sometimes_never == ["c-sometimes", "go-sometimes", "java-sometimes", "python-sometimes", "rust-sometimes", "sh-sometimes"]' checked.json
        touch $out
      '';

  # checks.gdb-languages: `rewind gdb` at a print of a program in each of
  # C++, Rust, Go, Python and Ruby, run from the store as a Nix build's
  # programs are, shows what gdb shows a live process of each: C++'s
  # standard library printers, a thread-local of the main thread and of a
  # worker blocked off the CPU, and errno, through the shared libraries the
  # loader listed; Rust's printers; the Go runtime's goroutines; CPython's
  # py-bt; and Ruby's interpreter by its symbols. Boots the VM, so it needs
  # /dev/kvm.
  gdb-languages =
    pkgs.runCommand "rewind-gdb-languages"
      {
        nativeBuildInputs = [
          rewind
          gdb
          # Rust's printers, from the rustc on PATH as rust-gdb finds them:
          # the sandbox has no Nix database to name the one that built it.
          pkgs.rustc
        ];
        requiredSystemFeatures = [ "kvm" ];
      }
      ''
        export REWIND_HOME=$TMPDIR/rewind
        # No debuginfod server: without a network, each of gdb's questions
        # to it waits out a timeout. libpython's and libc's DWARF come from
        # their debug outputs instead.
        export REWIND_DEBUGINFOD=/nonexistent
        debug='set debug-file-directory ${pkgs.python3.debug}/lib/debug:${pkgs.glibc.debug}/lib/debug'

        rewind run -q --clock exits --name languages --root ${languagesRoot} -- /bin/languages

        # gdb at the step of a language's first print, with these commands.
        at() {
          language=$1
          shift
          step=$(rewind log --steps languages | grep "hello from $language, alice" | awk '{print $1}')
          rewind gdb languages "$step" -- -batch -iex "$debug" "$@" > gdb 2>&1 || true
        }

        # What gdb must have printed, and must not have.
        has() {
          if ! grep -qF -- "$2" gdb; then
            echo "$1: no '$2' in:"
            cat gdb
            exit 1
          fi
        }
        lacks() {
          if grep -qF -- "$2" gdb; then
            echo "$1: '$2' in:"
            cat gdb
            exit 1
          fi
        }

        at c++ -ex 'frame function Greeter::greet' -ex 'p *this' \
          -ex 'printf "main calls %d\n", calls' -ex 'printf "errno %d\n", errno' \
          -ex 'thread 3' -ex 'printf "worker calls %d\n", calls'
        has c++ 'std::vector of length 4'
        has c++ 'main calls 1'
        has c++ 'errno 0'
        has c++ 'worker calls 7'

        at rust -ex 'frame function greet::Greeter::greet' -ex 'p *self'
        has rust 'counts: Vec(size=4) = {1, 2, 3, 5}'
        lacks rust 'Python Exception'

        at go -ex 'info goroutines' -ex 'frame function main.(*Greeter).Greet' -ex 'p *g' \
          -ex 'thread 3' -ex 'p/x $fs_base'
        has go 'runtime.gopark'
        has go 'Counts = []int = {1, 2, 3, 5}'
        lacks go '<unavailable>'

        at python -ex py-bt
        has python 'line 9, in greet'

        at ruby -ex bt
        has ruby 'ruby_run_node'
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
  # binary cache, sets the AMD workaround at boot with its marker in the
  # Nix sandbox, and adds the `rewind-amd` system feature beside NixOS's
  # own, which it must not replace. Only evaluates, so it needs no KVM.
  module =
    let
      config = moduleSystem.config;
      installed = map (p: p.name) config.environment.systemPackages;
    in
    assert builtins.elem rewind.name installed;
    assert builtins.any (name: pkgs.lib.hasPrefix "rewind-app-" name) installed;
    assert builtins.elem "msr" config.boot.kernelModules;
    assert builtins.elem "https://rewindvm.cachix.org" config.nix.settings.extra-substituters;
    assert config.nix.settings.extra-system-features == [ "rewind-amd" ];
    assert config.nix.settings.extra-sandbox-paths == [ "/run/rewind?" ];
    assert builtins.elem "kvm" config.nix.settings.system-features;
    # The nix.conf the system would install, which NixOS checks with Nix
    # itself when it builds the file.
    pkgs.writeText "rewind-module" ''
      ${config.systemd.services.rewind-pmu.serviceConfig.ExecStart}
      ${config.environment.etc."nix/nix.conf".source}
    '';
}
