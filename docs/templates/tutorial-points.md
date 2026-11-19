# Tutorial: schedule points

<!-- let epoch = 1790985600 -->
<!-- let repo = github:fzakaria/rewindvm -->

`rewind check` switches threads where the program makes a system call, and
nowhere else. A race between two system calls is in its reach, as the
[properties tutorial](tutorial-properties.md)'s `bank` is: each deposit
writes a ledger line between reading the balance and storing it. A race
with nothing between the read and the store is not, and no number of
schedules finds it. A schedule point is one line from the Rewind SDK that
puts a switch there.

The example is `counter` from
[examples](https://github.com/fzakaria/rewindvm/tree/main/examples): two
threads count hits on one counter, and each hit reads the count and stores
the count plus one. Install Rewind as the [Nix tutorial](tutorial-nix.md)
does.

## The race no schedule reaches

`counter` asserts with the SDK that the total adds up, so check judges each
run by the `sdk` property. Under 64 schedules, every run holds:

```console run name=plain show=-2:
$ rewind check --epoch {{epoch}} {{repo}}#counter
```

<!-- assert: grep -q '^every property held under all 65 schedules$' {{out:plain}} -->

The threads do race. On a multicore host they run at the same instant, and
one hit in a while lands between the other thread's read and store. In the
VM they take turns on one CPU, and a thread gives the CPU up only at a
system call: thread creation, the join at the end, nothing inside the loop.
Each thread counts its ten hits in one stretch, and the total is always
twenty.

## Mark the place

Clone the repository to edit `counter`:

```console run hide
$ git clone https://github.com/fzakaria/rewindvm
$ cd rewindvm
```

In `examples/counter/counter.c`, put a schedule point between the read and
the store:

<!-- diff: counter-point.patch -->
<!-- run: patch -p1 < {{dir}}/counter-point.patch -->

`rewind_point()` makes one exit from the VM, where a schedule can switch to
another thread or hold this one up, as at any system call. It writes
nothing to the run, and outside Rewind it does nothing.

Now check finds a schedule that loses hits, and narrows it to the point
that decides it:

```console run name=found
$ rewind check --epoch {{epoch}} .#counter | grep -E 'decides it|passing:|failing:|failing run violates sdk'
```

<!-- capture passing: passing: run (\w+) -->
<!-- capture failing: failing: run (\w+) -->
<!-- capture deciding: step (\d+) decides it -->
<!-- capture assert_step: failing run violates sdk: step (\d+): -->
<!-- capture lost_total: always "hits-add-up" failed: (\d+) hits -->
<!-- assert: grep -qE '^step [0-9]+ decides it: a request to switch threads there makes the run violate same-outcome, sdk$' {{out:found}} -->
<!-- assert: grep -qE 'failing run violates sdk: step [0-9]+: always "hits-add-up" failed: 1?[0-9] hits, expected 20' {{out:found}} -->

<!-- set from: rewind threads {{failing}} --to {{deciding}} | tail -1 | awk '{print $1}' -->
<!-- set to: echo $(( {{deciding}} + 15 )) -->

The run violates `same-outcome` too, since the job's build fails where
schedule 0's passed. `rewind threads` shows which thread held the CPU from
the stretch the deciding step falls in, in the failing run and in the
passing one it was forked from:

```console run name=threads
$ rewind threads {{failing|short}} --from {{from}} --to {{to}}
```

<!-- set cut_thread: sed -n 1p {{out:threads}} | awk '{print $3}' -->
<!-- set cut_step: sed -n 1p {{out:threads}} | awk '{print $1}' -->
<!-- set other_thread: awk -v cut={{cut_thread}} 'NR > 1 && $3 != cut && $4 == "counter" {print $3; exit}' {{out:threads}} -->
<!-- set switch_step: awk -v t={{other_thread}} '$3 == t {print $1; exit}' {{out:threads}} -->
<!-- assert: sed -n 1p {{out:threads}} | awk -v d={{deciding}} '{exit !($4 == "counter" && $2 == d + 1)}' -->
<!-- assert: test -n "{{other_thread}}" && test {{switch_step}} -gt {{cut_step}} -->
<!-- assert: rewind events {{failing}} | grep -E 'thread exit\(counter\)' | head -1 | grep -q ' {{other_thread}} ' -->

```console run name=threads_passing
$ rewind threads {{passing|short}} --from {{from}} --to {{to}}
```

<!-- assert: sed -n 1p {{out:threads_passing}} | awk -v t={{cut_thread}} -v d={{deciding}} '{exit !($3 == t && $2 > d + 1)}' -->
<!-- assert: rewind events {{passing}} | grep -E 'thread exit\(counter\)' | head -1 | grep -q ' {{cut_thread}} ' -->

In the passing run thread {{cut_thread}} takes the CPU at step {{cut_step}}
and keeps it until it has counted its hits and exits. In the failing run the
switch at step {{deciding}} gives it one step, to its first point: it has
read the count and not stored it. Thread {{other_thread}} then takes the CPU
at step {{switch_step}} and counts every one of its hits, and when thread
{{cut_thread}} gets the CPU back it stores the count it read, over all of
them. Open the failing run beside the passing one, at the point, with the
thread lanes shown and zoomed in on the window around it:

```console
$ rewind open {{failing|short}} {{deciding}} --compare {{passing|short}}
```

<!-- screenshot site/img/app-point: {{home}}/runs/{{failing}} --compare {{home}}/runs/{{passing}} --step {{deciding}} ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key plus ;; key t ;; wait 2 ;; click 1060 327 ;; click 1372 327 ;; wait 2 -->

![The app at step {{deciding}} of the failing counter run, compared with the passing one, with the Threads panel open: a lane per thread, the playhead at the deciding step, thread {{cut_thread}} on the CPU until one step past it and the other counter thread from step {{switch_step}}](../site/img/app-point.png)

The Threads panel has a lane per thread, with the stretches each held the
CPU: the failing run's above, the passing run's in blue under them. In the
failing run, thread {{cut_thread}}'s lane stops one step past the deciding
step and the other `counter` thread's lane runs until its hits are counted;
in the passing run, thread {{cut_thread}} keeps the CPU until it is done.

## Check the fix

Store only if the count is still what was read, and read it again if not:

<!-- diff: counter-fix.patch -->
<!-- run: patch -p1 < {{dir}}/counter-fix.patch -->

The point stays. A switch there now finds the count changed, and the
compare and exchange reads it again instead of storing over it:

```console run name=fixed
$ rewind check --epoch {{epoch}} .#counter | tail -1
```

<!-- assert: grep -q '^every property held under all 65 schedules$' {{out:fixed}} -->

## Where a point goes

A point goes where a switch would matter and no system call happens:
between a read and the store computed from it, in a lock-free structure's
push or pop, at the top of a retry loop. A codebase that wraps its atomics
in macros of its own gets a point at every atomic by adding one line to
that header. Each point costs one exit per visit, so a run with a point in
a hot loop is longer and slower, and a point in code that runs outside
Rewind costs one failed `open` the first time and nothing after.

The call is `rewind_point()` in C and C++, `rewind_sdk::point()` in Rust,
`rewind.Point()` in Go and `rewind_sdk.point()` in Python. The VM has one
CPU, so points find the orders of operations that lose an update or act on
a stale check. A bug that needs two CPUs to reorder memory is out of reach.

## What to read next

- [The properties tutorial](tutorial-properties.md): the `sdk` property and
  the assertions `counter` makes.
- [The SDK](https://github.com/fzakaria/rewindvm/tree/main/sdk): every
  call, in every language.
- [Design](design.md#limits): where the VM switches threads and why.
