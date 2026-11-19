# Case study: a hang in Nix's store schema migration

A Nix master build from April 2026 could hang when several `nix` commands
opened a new store at once, printing `SQLite database ... is busy` every ten
seconds. It was reported as
[NixOS/nix#15693](https://github.com/NixOS/nix/issues/15693) and fixed upstream
in two steps, not by us. Rewind VM reproduced it, gdb in the VM pinned the
hang to `SQLITE_BUSY_SNAPSHOT`, and checks showed why the first fix,
[#15694](https://github.com/NixOS/nix/pull/15694), left the hang in place and
the second, [#15967](https://github.com/NixOS/nix/pull/15967), removed it.

Scrub the recording in your browser: [open it in the viewer](https://rewindvm.dev/viewer/?url=examples/nix-schema-migration-hang.rwd).

## The code

On a new store, every `nix` process runs the schema migrations while holding
the store's big lock in shared mode only, so several can migrate at once. At
[a94dee99e](https://github.com/NixOS/nix/blob/a94dee99e1805b1df24daefcdfa86a3d50c63685/src/libstore/local-store.cc#L566-L601),
the version the issue names, each migration runs in its own transaction:

```cpp
    auto doUpgrade = [&](const std::string & migrationName, const std::string & stmt) {
        if (schemaMigrations.contains(migrationName))
            return;

        debug("executing Nix database schema migration '%s'...", migrationName);

        SQLiteTxn txn(state.db);
        state.db.exec(stmt + fmt(";\ninsert into SchemaMigrations values('%s')", migrationName));
        txn.commit();

        schemaMigrations.insert(migrationName);
    };
```

`SQLite::exec`
([sqlite.cc](https://github.com/NixOS/nix/blob/a94dee99e1805b1df24daefcdfa86a3d50c63685/src/libstore/sqlite.cc#L130-L136))
retries its statement with no limit for as long as SQLite says the database is
busy.

## Run it

The test is
[ca/concurrent-builds.sh](https://github.com/NixOS/nix/blob/a94dee99e1805b1df24daefcdfa86a3d50c63685/tests/functional/ca/concurrent-builds.sh):
six `nix build` processes on an empty store. The derivation is Nix's
`nix-functional-tests` at a94dee99e, narrowed to this test, with gdb in its
inputs ([flake](../../examples/case-studies/flake.nix)). `--epoch` fixes the
VM's clock, which seeds its randomness, so these are the runs you get:

```console
$ rewind check --epoch 1791936000 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds-15693-gdb'
rewind: packing 260 store paths for nix-functional-concurrent-builds-gdb-2.35.0pre20260414_a94dee9
schedule   0: exited:0            32467 steps  a50a5ab6d992  run 6ef6905607aba8dd
schedule   1: exited:0            42316 steps  a50a5ab6d992  run d5e7fe95162a7c7f
...
schedule  13: exited:1           446805 steps    run 91470ebfb0738630
...
schedule  23: exited:1           445542 steps    run 2eb94546730c6f5e
...
schedule  35: exited:1           445987 steps    run de8b32a805655029
...
schedule  37: exited:1           443964 steps    run 9031a88c81d3cf99
...
schedule  49: exited:1           447119 steps    run 1d03fdbf588b3cc3
schedule  50: exited:1           446576 steps    run 24ab5bbfd60f8b0e
...
schedule 101: exited:1           443136 steps    run 92eb9cc8e9eebe18
...
schedule 110: exited:1           593426 steps    run fb2c0e958fc1b1e2
...
schedule 112: exited:1           444549 steps    run fb628815c249c7cb
...
schedule 160: exited:1           444936 steps    run 2e0b7d8b6268a26c
...
schedule 175: exited:1           444135 steps    run 111dde44790febe5
...
schedule 183: exited:1           443985 steps    run b55d9ac5ca7bea02
...
schedule 185: exited:1           445823 steps    run 90b3ec2da1dbda0b
...
schedule 195: exited:1           598095 steps    run ed7f9d2556bb636a
...
schedule 220: exited:1           444160 steps    run 7ccadcfb63b65635
...
schedule 243: exited:1           446335 steps    run 4d4d30386f1d32fc
...
16 of 256 other schedules ended differently

schedule 13 ends differently from schedule 0; finding the step that decides it
schedule 13 limited to steps 20994..22502 still ends differently
step 22501 decides it: a request to switch threads there makes the run fail

passing: run ac009cc3b40bd851, schedule 13 over steps 20994..22501
failing: run 6e0ec5d4d6ed8595, schedule 13 over steps 20994..22502
the two are the same run until step 22501

where /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh first behaves differently:
  both          18081   188/188   fork() = 225
  both          18084   188/188   fork() = 226
  both          33288   188/188   SIGCHLD code=1 addr=0x0
  failing      266609   188/188   SIGTERM code=0 addr=0x0
  failing      266610   188/188   exit_group(bash) killed:SIGTERM
  passing       30607   188/188   SIGCHLD code=1 addr=0x0
  passing       30608   188/188   exit_group(bash) exited:0

open both in the desktop app: rewind open 6e0ec5d4d6ed8595 266609 --compare ac009cc3b40bd851
```

Schedule 0 passes and 16 of the other 256 fail. The step that decides
schedule 13, 22501, falls inside the race, after the test
starts its first `nix build` at step 18087: one `nix` process is
executing an SQLite statement there, and a single switch at that moment
decides whether all six get through the migrations. On the host the test
passed 51 of 51 runs.

## What the failing run shows

Schedule 13 ends with meson's 300 second timeout, reached in seconds
of wall time since the VM skips ahead while it sleeps:

```console
$ rewind log 91470ebf | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      1 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.03s   killed by signal 15 SIGTERM
      1 1/1 nix-functional-tests:ca / concurrent-builds        TIMEOUT        300.03s   killed by signal 15 SIGTERM
      2        insert into SchemaMigrations values('20251017-ca-derivations')': constraint failed, UNIQUE constraint failed: SchemaMigrations.migration (in '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite')
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy

$ rewind events 91470ebf | grep -E 'exit_group\(nix\)'
     27409   226/230   exit_group(nix) exited:1
     28253   224/235   exit_group(nix) exited:1
     33050   239/239   exit_group(nix) killed:SIGTERM
     38106   225/233   exit_group(nix) exited:0
     41214   221/228   exit_group(nix) exited:0
     41713   222/227   exit_group(nix) exited:0
    446618   223/229   exit_group(nix) exited:1
```

Both failures from the issue are in this run. 226 and 224 lost the race to
record a migration and exited with the UNIQUE error. 223 hung until meson
killed the test at step 446618. (239 is the build hook, which often
ends with SIGTERM in passing runs too.)

## Where the hung process is

At step 200000, with the VM idle while 223 sleeps between two
retries, only 223 is left, and it holds every lock:

```console
$ rewind ps 91470ebf 200000
     1 /init
    34   bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   187     /nix/store/...-python3-3.13.11/bin/python3.13 /nix/store/...-meson-1.9.1/bin/meson test --no-rebuild --print-errorlogs concurrent-builds
   188       /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh
   223         nix build --no-link --file ./racy.nix
   229           (thread)

$ rewind cat 91470ebf 200000 /proc/locks
1: POSIX  ADVISORY  READ 223 00:03:1812 124 124
2: POSIX  ADVISORY  READ 223 00:03:1812 128 128
3: POSIX  ADVISORY  READ 223 00:03:1809 1073741826 1073742335
4: FLOCK  ADVISORY  READ 223 00:03:1808 0 EOF
```

223 holds the big lock shared (inode 1808), SQLite's lock on the
database (1809) and a read mark in the WAL index (1812). It is
inside a read transaction, and nothing else is in its way.

## Ask gdb

`rewind shell` opens a shell in a throwaway fork of the run at a step. gdb
attaches to 223 there, stops it where it handles SQLite's next busy
answer, and catches the retry after that. The shell reads these
lines from [gdb.sh](../templates/nix-schema-migration-hang-gdb.sh); typed in, they do the same:

```console
$ rewind shell 91470ebf 200000 --pid 223 < gdb.sh
rewind: a shell at step 200000 of 91470ebfb0738630; exit it to leave
[rewind] /build/source/tests/functional/ca # cat > /tmp/g.cmd <<'END'
> set pagination off
> handle SIGSTOP nostop noprint nopass
> set unwind-on-signal on
> break nix::handleSQLiteBusy
> continue
> bt
> delete
> break sqlite3_exec
> continue
> set $db = $rdi
> printf "statement: %.200s\n", (char *) $rsi
> finish
> printf "sqlite3_exec returned %d\n", (int) $rax
> printf "extended error code %d\n", (int) sqlite3_extended_errcode($db)
> printf "errmsg %s\n", (char *) sqlite3_errmsg($db)
> printf "autocommit %d\n", (int) sqlite3_get_autocommit($db)
> detach
> END
[rewind] /build/source/tests/functional/ca # gdb -p 223 -batch -x /tmp/g.cmd 2>&1 | grep -vE "^warning|auto-load|add-auto-load|line to your|To (enable|completely)|set auto-load|For more information|info .\(gdb\)|^$"
[New LWP 229]
...
Breakpoint 1 at 0x7fa99ed3fb76
Thread 1 "nix" hit Breakpoint 1, 0x00007fa99ed3fb76 in nix::handleSQLiteBusy(nix::SQLiteBusy const&, long&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#0  0x00007fa99ed3fb76 in nix::handleSQLiteBusy(nix::SQLiteBusy const&, long&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#1  0x00007fa99ebe88d5 in void nix::retrySQLite<void, nix::SQLite::exec(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&)::{lambda()#1}>(nix::SQLite::exec(std::__cxx11::basic_string< ...
#2  0x00007fa99ece637e in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&)::{lambda(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&, std::__cxx11::basic_string<char, std::c ...
#3  0x00007fa99ece88a9 in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#4  0x00007fa99ece9605 in nix::LocalStore::LocalStore(nix::ref<nix::LocalStoreConfig const>) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#5  0x00007fa99ece3046 in nix::LocalStoreConfig::openStore() const () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#6  0x00007fa99e429ddc in nix::StoreCommand::createStore() () from /nix/store/...-nix-cmd-2.35.0pre/lib/libnixcmd.so.2.35.0
#7  0x00007fa99e429edc in nix::StoreCommand::getStore() () from /nix/store/...-nix-cmd-2.35.0pre/lib/libnixcmd.so.2.35.0
...
Breakpoint 2 at 0x7fa99d1ffa68
Thread 1 "nix" hit Breakpoint 2, 0x00007fa99d1ffa68 in sqlite3_exec () from /nix/store/...-sqlite-3.50.4/lib/libsqlite3.so
statement: drop index if exists IndexReferrer;
insert into SchemaMigrations values('20260309-drop-redundant-indexreferrer')
...
sqlite3_exec returned 5
extended error code 517
errmsg database is locked
autocommit 0
[Inferior 1 (process 223) detached]
[rewind] /build/source/tests/functional/ca # exit
```

223 is retrying the second migration. SQLite returns `SQLITE_BUSY` (5)
with the extended code 517, `SQLITE_BUSY_SNAPSHOT`, and `autocommit 0` says
the connection is still inside the migration's transaction.

## Root cause

`SQLITE_BUSY_SNAPSHOT`
([SQLite docs](https://www.sqlite.org/rescode.html#busy_snapshot)) is what a WAL connection gets when it tries to write after another connection has
committed since its read. Its snapshot stays stale until the transaction ends.

223 began the migration and read the schema, which fixed its snapshot.
Another `nix` process committed a migration. 223's write was refused, and
`SQLite::exec` retried the statement inside the same transaction, on the same
stale snapshot, forever: 28 warnings, ten seconds apart. A process whose
read came before the other commit but whose snapshot was still current ran
the migration again and failed with the UNIQUE error instead.

## Check the two fixes

[#15694](https://github.com/NixOS/nix/pull/15694) changed the insert to
`insert or ignore`. That removes the UNIQUE error, but not the stale snapshot.
At its merge commit
[c390460cd](https://github.com/NixOS/nix/commit/c390460cdf7ee8b3208d982e09f91555f980759e):

```console
$ rewind check --epoch 1791936000 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds-15694'
rewind: packing 216 store paths for nix-functional-concurrent-builds-2.35.0pre20260416_c390460
schedule   0: exited:0            32531 steps  a50a5ab6d992  run e2664ff2b7a7bf56
...
schedule   2: exited:1           446867 steps    run 5e2a818760d897cf
...
schedule  14: exited:1           450009 steps    run ff4f463f4fd27b27
...
schedule  35: exited:1           448503 steps    run b4fe8efc0ce25b6c
...
schedule  41: exited:1           606457 steps    run bcdd9b05d05d9357
...
schedule  46: exited:1           453963 steps    run cd5a3909585fc86f
...
schedule  52: exited:1           450959 steps    run c52d1e1b79f70864
...
schedule  80: exited:1           449879 steps    run bb4417f3fb259728
...
schedule 126: exited:1           446834 steps    run ff9f4d7398dd6e39
...
schedule 131: exited:1           749588 steps    run 907f44d28e95ec69
...
schedule 185: exited:1           756389 steps    run edc247d659a954c8
...
schedule 191: exited:1           449152 steps    run 76cde80f187e7388
...
schedule 200: exited:1           447138 steps    run 8e32323738e72a40
...
12 of 256 other schedules ended differently
...
```

```console
$ rewind log 5e2a8187 | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      1 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.00s   killed by signal 15 SIGTERM
      1 1/1 nix-functional-tests:ca / concurrent-builds        TIMEOUT        300.00s   killed by signal 15 SIGTERM
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy
```

All 12 failing runs hang the same way, with no UNIQUE error.
[#15967](https://github.com/NixOS/nix/pull/15967) takes the big lock
exclusively while a migration runs, so no other process can commit during its
transaction. Nix 2.35.2 from nixpkgs has both fixes and passes every schedule:

```console
$ rewind check --epoch 1791936000 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds'
rewind: packing 217 store paths for nix-functional-concurrent-builds-2.35.2
schedule   0: exited:0            37020 steps  a50a5ab6d992  run 3593d4459a7dfe7c
...
0 of 256 other schedules ended differently
same result under all 257 schedules
```

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-schema-migration-hang.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd)
  (221.5 KB), the trace, for `rewind events`, `rewind log` and the app; the
  [viewer](https://rewindvm.dev/viewer/?url=examples/nix-schema-migration-hang.rwd) opens it in a browser.
- [nix-schema-migration-hang-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd)
  (436.6 MB), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd
$ rewind replay 91470ebf
$ rewind shell 91470ebf <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd
```
