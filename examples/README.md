# Examples

## mylib

A small C thread pool with a shutdown race, for the Rewind VM tutorials.

`pool_shutdown` frees the job queue before joining the workers, and a worker
that has finished a job checks whether the pool is stopping without holding
the lock, then counts the job through the queue. When shutdown runs between
that check and the count, the worker writes through a null queue pointer and
`test_pool_shutdown` dies with SIGSEGV. On a 16 core laptop that happens in
roughly one build in ten.

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

```console
$ make -C bank check                       # plain, on the host
$ nix build github:fzakaria/rewindvm#bank  # as a derivation
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
