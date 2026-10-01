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
