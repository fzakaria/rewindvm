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
schedule   0: exited:0            26742 steps  a50a5ab6d992  run 9e1c61fcbbe3896d
schedule   1: exited:0            42993 steps  a50a5ab6d992  run d6a55d1a003577ea
...
schedule  74: exited:1           446081 steps    run c345231ded12ccdb
...
schedule 160: exited:1           598064 steps    run b239526b89c4aed9
...
schedule 173: exited:1           445286 steps    run 25c70fda22a6f3ab
...
schedule 179: exited:1           447096 steps    run ec3edd8a6571d5ff
...
schedule 184: exited:1           445023 steps    run 4c10d0bb364b8d08
...
schedule 188: exited:1           589498 steps    run 7911bfcb0d36d694
...
schedule 191: exited:1           597407 steps    run 53478121045f9401
...
schedule 225: exited:1           741336 steps    run d47f09440fb32a44
schedule 226: exited:1           445082 steps    run 825c2f01a7fc9ecc
...
schedule 236: exited:1           444351 steps    run 94410beef8b48cbf
...
10 of 256 other schedules ended differently

schedule 74 ends differently from schedule 0; finding the step that decides it
schedule 74 limited to steps 714..4229 still ends differently
step 4228 decides it: a request to switch threads there makes the run fail

passing: run 5c2b23f8bc393fec, schedule 74 over steps 714..4228
failing: run 35a345b742f116d1, schedule 74 over steps 714..4229
the two are the same run until step 4228

where /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh first behaves differently:
  both          19205   188/188   fork() = 223
  both          19208   188/188   fork() = 224
  both          19211   188/188   fork() = 225
  failing      267856   188/188   SIGTERM code=0 addr=0x0
  failing      267857   188/188   exit_group(bash) killed:SIGTERM
  passing       31053   188/188   SIGCHLD code=1 addr=0x0
  passing       31966   188/188   SIGCHLD code=1 addr=0x0
  passing       32862   188/188   SIGCHLD code=1 addr=0x0
  passing       32867   188/188   exit_group(bash) exited:0

open both in the desktop app: rewind open 35a345b742f116d1 267856 --compare 5c2b23f8bc393fec
```

Schedule 0 passes and 10 of the other 256 fail. The step that decides
schedule 74, 4228, falls while the build is still copying its
sources with `cp`, long before the test starts its first `nix build` at step
19165: one switch there moves every step after it, and the six `nix`
processes race from the moment they start. On the host the test passed 51 of
51 runs.

## What the failing run shows

Schedule 226 ends with meson's 300 second timeout, reached in seconds
of wall time since the VM skips ahead while it sleeps:

```console
$ rewind log 825c2f01 | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      1 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.05s   killed by signal 15 SIGTERM
      1 1/1 nix-functional-tests:ca / concurrent-builds        TIMEOUT        300.05s   killed by signal 15 SIGTERM
      2        insert into SchemaMigrations values('20251017-ca-derivations')': constraint failed, UNIQUE constraint failed: SchemaMigrations.migration (in '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite')
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy

$ rewind events 825c2f01 | grep -E 'exit_group\(nix\)'
     26492   225/233   exit_group(nix) exited:1
     30806   222/230   exit_group(nix) exited:1
     31741   238/238   exit_group(nix) killed:SIGTERM
     36725   224/232   exit_group(nix) exited:0
     40009   221/227   exit_group(nix) exited:0
     40519   223/234   exit_group(nix) exited:0
    444904   220/226   exit_group(nix) exited:1
```

Both failures from the issue are in this run. 225 and 222 lost the race to
record a migration and exited with the UNIQUE error. 220 hung until meson
killed the test at step 444904. (238 is the build hook, which often
ends with SIGTERM in passing runs too.)

## Where the hung process is

At step 200006, with the VM idle while 220 sleeps between two
retries, only 220 is left, and it holds every lock:

```console
$ rewind ps 825c2f01 200006
     1 /init
    34   bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   187     /nix/store/...-python3-3.13.11/bin/python3.13 /nix/store/...-meson-1.9.1/bin/meson test --no-rebuild --print-errorlogs concurrent-builds
   188       /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh
   220         nix build --no-link --file ./racy.nix
   226           (thread)

$ rewind cat 825c2f01 200006 /proc/locks
1: POSIX  ADVISORY  READ 220 00:03:1812 124 124
2: POSIX  ADVISORY  READ 220 00:03:1812 128 128
3: POSIX  ADVISORY  READ 220 00:03:1809 1073741826 1073742335
4: FLOCK  ADVISORY  READ 220 00:03:1808 0 EOF
```

220 holds the big lock shared (inode 1808), SQLite's lock on the
database (1809) and a read mark in the WAL index (1812). It is
inside a read transaction, and nothing else is in its way.

## Ask gdb

`rewind shell` opens a shell in a throwaway fork of the run at a step. gdb
attaches to 220 there, stops it where it handles SQLite's next busy
answer, and catches the retry after that. The shell reads these
lines from [gdb.sh](../templates/nix-schema-migration-hang-gdb.sh); typed in, they do the same:

```console
$ rewind shell 825c2f01 200006 --pid 220 < gdb.sh
rewind: a shell at step 200006 of 825c2f01a7fc9ecc; exit it to leave
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
[rewind] /build/source/tests/functional/ca # gdb -p 220 -batch -x /tmp/g.cmd 2>&1 | grep -vE "^warning|auto-load|add-auto-load|line to your|To (enable|completely)|set auto-load|For more information|info .\(gdb\)|^$"
[New LWP 226]
...
Breakpoint 1 at 0x7febfa879b76
Thread 1 "nix" hit Breakpoint 1, 0x00007febfa879b76 in nix::handleSQLiteBusy(nix::SQLiteBusy const&, long&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#0  0x00007febfa879b76 in nix::handleSQLiteBusy(nix::SQLiteBusy const&, long&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#1  0x00007febfa7228d5 in void nix::retrySQLite<void, nix::SQLite::exec(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&)::{lambda()#1}>(nix::SQLite::exec(std::__cxx11::basic_string< ...
#2  0x00007febfa82037e in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&)::{lambda(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&, std::__cxx11::basic_string<char, std::c ...
#3  0x00007febfa8228a9 in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#4  0x00007febfa823605 in nix::LocalStore::LocalStore(nix::ref<nix::LocalStoreConfig const>) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#5  0x00007febfa81d046 in nix::LocalStoreConfig::openStore() const () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#6  0x00007febf9f63ddc in nix::StoreCommand::createStore() () from /nix/store/...-nix-cmd-2.35.0pre/lib/libnixcmd.so.2.35.0
#7  0x00007febf9f63edc in nix::StoreCommand::getStore() () from /nix/store/...-nix-cmd-2.35.0pre/lib/libnixcmd.so.2.35.0
...
Breakpoint 2 at 0x7febf8d39a68
Thread 1 "nix" hit Breakpoint 2, 0x00007febf8d39a68 in sqlite3_exec () from /nix/store/...-sqlite-3.50.4/lib/libsqlite3.so
statement: drop index if exists IndexReferrer;
insert into SchemaMigrations values('20260309-drop-redundant-indexreferrer')
...
sqlite3_exec returned 5
extended error code 517
errmsg database is locked
autocommit 0
[Inferior 1 (process 220) detached]
[rewind] /build/source/tests/functional/ca # exit
```

220 is retrying the second migration. SQLite returns `SQLITE_BUSY` (5)
with the extended code 517, `SQLITE_BUSY_SNAPSHOT`, and `autocommit 0` says
the connection is still inside the migration's transaction.

## Root cause

`SQLITE_BUSY_SNAPSHOT`
([SQLite docs](https://www.sqlite.org/rescode.html#busy_snapshot)) is what a WAL connection gets when it tries to write after another connection has
committed since its read. Its snapshot stays stale until the transaction ends.

220 began the migration and read the schema, which fixed its snapshot.
Another `nix` process committed a migration. 220's write was refused, and
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
schedule   0: exited:0            32707 steps  a50a5ab6d992  run 581ace8d65200eb7
...
schedule   8: exited:1           446600 steps    run d27280d31e465f9c
...
schedule  18: exited:1           446168 steps    run ad31cc5e1ff1f254
...
schedule  67: exited:1           450849 steps    run 7773033bc7f49146
...
schedule  79: exited:1           447508 steps    run 4f6ec7f56acd7e33
...
schedule  84: exited:1           449277 steps    run 665939d208be0eb5
...
schedule  89: exited:1           450523 steps    run 5d159bab0ca6deeb
...
schedule 118: exited:1           448275 steps    run 8622336423a1a897
...
schedule 137: exited:1           450094 steps    run a7f47a0575b191f8
...
schedule 145: exited:1           449607 steps    run cd06f3c387613f48
...
schedule 191: exited:1           448319 steps    run a3816ff768981a1d
...
schedule 216: exited:1           612037 steps    run 4a65f422fa31c5ff
...
schedule 245: exited:1           452736 steps    run 921f9532e389688b
...
12 of 256 other schedules ended differently
...
```

```console
$ rewind log d27280d3 | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      1 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.03s   killed by signal 15 SIGTERM
      1 1/1 nix-functional-tests:ca / concurrent-builds        TIMEOUT        300.03s   killed by signal 15 SIGTERM
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy
```

All 12 failing runs hang the same way, with no UNIQUE error.
[#15967](https://github.com/NixOS/nix/pull/15967) takes the big lock
exclusively while a migration runs, so no other process can commit during its
transaction. Nix 2.35.2 from nixpkgs has both fixes and passes every schedule:

```console
$ rewind check --epoch 1791936000 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds'
rewind: packing 217 store paths for nix-functional-concurrent-builds-2.35.2
schedule   0: exited:0            36558 steps  a50a5ab6d992  run c93986601f228c7d
...
0 of 256 other schedules ended differently
same result under all 257 schedules
```

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-schema-migration-hang.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd)
  (135.3 KB), the trace, for `rewind events`, `rewind log` and the app; the
  [viewer](https://rewindvm.dev/viewer/?url=examples/nix-schema-migration-hang.rwd) opens it in a browser.
- [nix-schema-migration-hang-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd)
  (535.1 MB), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd
$ rewind replay 825c2f01
$ rewind shell 825c2f01 <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd
```
