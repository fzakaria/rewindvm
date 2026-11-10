# Examples

## mylib

A small C thread pool with a shutdown race, for the Rewind VM tutorials.

`pool_shutdown` frees the job queue before joining the workers, and a worker
that has finished a job checks whether the pool is stopping without holding
the lock, then counts the job through the queue. When shutdown runs between
that check and the count, the worker writes through a null queue pointer and
`test_pool_shutdown` dies with SIGSEGV. On a 16 core laptop that happens in
roughly one build in twenty.

```console
$ make -C mylib check                           # plain, on the host
$ nix build github:fzakaria/rewindvm#mylib     # as a derivation
$ docker build -t mylib -f mylib/Containerfile mylib   # as a container
```

The tutorials find the failing interleaving with `rewind check`, look at it
step by step, and check the fix:
[Nix](../docs/tutorial-nix.md) · [container](../docs/tutorial-container.md).

## philosophers

The dining philosophers. Five threads share five mutexes in a ring, and each
locks the one on its left, then the one on its right. When every thread holds
its left mutex at once, each waits for its neighbor forever. `make check`
gives dinner ten seconds and fails with timeout's status 124 when it
deadlocks. Locking the lower numbered fork first fixes it.

```console
$ make -C philosophers check                       # plain, on the host
$ nix build github:fzakaria/rewindvm#philosophers  # as a derivation
```

## bank

Two tellers deposit into one account from two threads. A deposit reads the
balance, writes a line to the ledger, then stores the balance plus the
deposit, with no lock, so a teller that runs between the other's read and
store has its deposit written over. `make check` fails when the balance
comes up short. Holding a mutex across the read and the store fixes it.
It asserts with the Rewind SDK, through a copy of
[`sdk/c/rewind.h`](../sdk/c/rewind.h): the balance adds up, and some run
should have both tellers mid-deposit at once.

```console
$ make -C bank check                       # plain, on the host
$ nix build github:fzakaria/rewindvm#bank  # as a derivation
```

## bank-java

`bank` in Java, as a JUnit 5 suite: one teller, two tellers, and a new
account. The two-tellers test loses a deposit the same way, and fails its
`assertEquals`. The suite is marked with the Rewind SDK's JUnit extension,
through a copy of [`sdk/java`](../sdk/java), so `rewind check` sees each
test's start and result, and JUnit writes its XML reports to `reports/`
for the `junit` property.

```console
$ make -C bank-java check JUNIT=junit-platform-console-standalone.jar  # plain, on the host
$ nix build github:fzakaria/rewindvm#bank-java                         # as a derivation
```

## counter

Two threads count hits on one counter. A hit reads the count and stores the
count plus one, with no lock and no system call between, so a thread that
runs between the other's read and store has its hit written over. `make
check` fails when the total comes up short. Rewind VM switches threads only
at system calls, so no schedule reaches this race until the schedule points
tutorial puts a `rewind_point()` between the read and the store. A compare
and exchange that reads again when the count has changed fixes it. It
asserts with the Rewind SDK, through a copy of
[`sdk/c/rewind.h`](../sdk/c/rewind.h), that the total adds up.

```console
$ make -C counter check                       # plain, on the host
$ nix build github:fzakaria/rewindvm#counter  # as a derivation
```

## config-reload

One process rewrites a config file in place, truncating it and writing its
two lines one at a time, while another process rereads it. A read between
the truncate and the last write sees a config with a line missing, and
`make check` fails. On a multicore host that happens nearly every time;
on Rewind VM's one CPU only some schedules land the reader there. Writing
a temporary file and renaming it over the config fixes it.

```console
$ make -C config-reload check                       # plain, on the host
$ nix build github:fzakaria/rewindvm#config-reload  # as a derivation
```

## waiter

A parent forks a worker and waits for it to exit: its SIGCHLD handler sets
a flag, and the parent checks the flag, logs that it is waiting, then calls
`pause`. A worker that exits after the check and before the `pause` sends
its signal into that gap, the parent pauses anyway, and nothing wakes it.
`make check` gives it ten seconds and fails with timeout's status 124.
Blocking SIGCHLD around the check and waiting with `sigsuspend` fixes it.

```console
$ make -C waiter check                       # plain, on the host
$ nix build github:fzakaria/rewindvm#waiter  # as a derivation
```

## hash-order

A post's tags, each once and lowercased, collected through a Rust
`HashSet`, and a test that expects them in the order its author saw. A
`HashSet` keeps no order, and Rust seeds its hasher from the system's
randomness, so the test passes in some runs and fails in others. In Rewind
VM the randomness is the seed's, so `rewind check --seeds` finds a seed it
fails under. Sorting the tags fixes it.

```console
$ make -C hash-order check                       # plain, on the host
$ nix build github:fzakaria/rewindvm#hash-order  # as a derivation
```

## tiles

The pkg-config file of a small library, written by a Python script that
collects the packages the library links against into a set. Python
iterates a set of strings in an order drawn from its hash seed, so the
`Requires.private` line comes out in another order from one build to the
next. nixpkgs pins the hash seed in builds with Python as an input, so the
derivation unsets it. `rewind check --seeds` builds different outputs, and
`rewind blame` names python3 as the program that wrote the differing bytes.
Sorting the packages fixes it.

```console
$ make -C tiles PREFIX=/tmp/tiles install   # plain, on the host
$ nix build github:fzakaria/rewindvm#tiles  # as a derivation
```

## plugins

An index of the plugins a package ships, written by a Python script in the
order `os.listdir` returns them. In Rewind VM a directory lists in the same
order in every run, so seeds do not change the output; the guest kernel's
`rewind.dirents=N` key lists directories in another order, as ext4 on
another machine would, and the index changes with it. Sorting the names
fixes it.

```console
$ make -C plugins PREFIX=/tmp/plugins install  # plain, on the host
$ nix build github:fzakaria/rewindvm#plugins   # as a derivation
```

The [reproducible builds tutorial](../docs/tutorial-repro.md) finds both.

## stamp

A program that prints the id and the time of its own build. The Makefile
reads a UUID from the kernel's random pool and the time from the clock as it
compiles, as release builds like to, so no two builds on an ordinary machine
make the same program, and `nix build --rebuild` says so. Inside Rewind VM
the pool's seed and the clock's start are inputs, and the flake's `inRewind`
writes them into the derivation.

```console
$ make -C stamp && ./stamp/stamp            # plain, on the host
$ nix build github:fzakaria/rewindvm#stamp  # as a derivation
```

The [deterministic derivations tutorial](../docs/tutorial-derivations.md)
builds it the same on every machine.
