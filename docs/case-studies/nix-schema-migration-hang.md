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
schedule   0: exited:0            26507 steps  a50a5ab6d992  run 362c2d1c38de4272
schedule   1: exited:0            45104 steps  a50a5ab6d992  run 1446e8f9bb77fe33
...
schedule   9: exited:1           446347 steps    run 3d0a28b5924960cf
...
schedule  29: exited:1           442196 steps    run 992c0739667aa813
...
schedule  37: exited:1           448138 steps    run 037f1491fe286cc3
...
schedule  44: exited:1           444720 steps    run 6a51bb095b506bdb
...
schedule  59: exited:1           447070 steps    run 0547f3997a0087e5
...
schedule  89: exited:1           444006 steps    run 6b8a60e10033ccae
...
schedule  94: exited:1           448442 steps    run e854022a09f469a2
...
schedule 131: exited:1           600492 steps    run ad422fb94e49d35f
...
schedule 133: exited:1           447881 steps    run 566d03b2c324e3d0
...
schedule 154: exited:1           444436 steps    run 7acaf6b83cd28114
...
schedule 171: exited:1           447323 steps    run f5bc3d18063bced9
...
11 of 256 other schedules ended differently

schedule 9 ends differently from schedule 0; finding the step that decides it
schedule 9 limited to steps 1896..28417 still ends differently
step 28416 decides it: a request to switch threads there makes the run fail

passing: run 1541a7ff04c19695, schedule 9 over steps 1896..28416
failing: run 1340d548fca9360f, schedule 9 over steps 1896..28417
the two are the same run until step 28416

where /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh first behaves differently:
  both          22567   188/188   fork() = 224
  both          22570   188/188   fork() = 225
  both          22573   188/188   fork() = 226
  failing      271281   188/188   SIGTERM code=0 addr=0x0
  failing      271282   188/188   exit_group(bash) killed:SIGTERM
  passing       34895   188/188   SIGCHLD code=1 addr=0x0
  passing       36245   188/188   SIGCHLD code=1 addr=0x0
  passing       36370   188/188   SIGCHLD code=1 addr=0x0
  passing       36371   188/188   exit_group(bash) exited:0

open both in the desktop app: rewind open 1340d548fca9360f 271281 --compare 1541a7ff04c19695
```

Schedule 0 passes and 11 of the other 256 fail. The step that decides
schedule 9, 28416, falls inside the race, after the test
starts its first `nix build` at step 22535: one `nix` process is
executing an SQLite statement there, and a single switch at that moment
decides whether all six get through the migrations. On the host the test
passed 51 of 51 runs.

## What the failing run shows

Schedule 9 ends with meson's 300 second timeout, reached in seconds
of wall time since the VM skips ahead while it sleeps:

```console
$ rewind log 3d0a28b5 | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      1 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.03s   killed by signal 15 SIGTERM
      1 1/1 nix-functional-tests:ca / concurrent-builds        TIMEOUT        300.03s   killed by signal 15 SIGTERM
      2        insert into SchemaMigrations values('20251017-ca-derivations')': constraint failed, UNIQUE constraint failed: SchemaMigrations.migration (in '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite')
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy

$ rewind events 3d0a28b5 | grep -E 'exit_group\(nix\)'
     27007   225/235   exit_group(nix) exited:1
     27351   226/236   exit_group(nix) exited:1
     32414   239/239   exit_group(nix) killed:SIGTERM
     37351   222/232   exit_group(nix) exited:0
     40752   224/231   exit_group(nix) exited:0
     41041   223/228   exit_group(nix) exited:0
    446180   221/227   exit_group(nix) exited:1
```

Both failures from the issue are in this run. 225 and 226 lost the race to
record a migration and exited with the UNIQUE error. 221 hung until meson
killed the test at step 446180. (239 is the build hook, which often
ends with SIGTERM in passing runs too.)

## Where the hung process is

At step 200000, with the VM idle while 221 sleeps between two
retries, only 221 is left, and it holds every lock:

```console
$ rewind ps 3d0a28b5 200000
     1 /init
    34   bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   187     /nix/store/...-python3-3.13.11/bin/python3.13 /nix/store/...-meson-1.9.1/bin/meson test --no-rebuild --print-errorlogs concurrent-builds
   188       /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh
   221         nix build --no-link --file ./racy.nix
   227           (thread)

$ rewind cat 3d0a28b5 200000 /proc/locks
1: POSIX  ADVISORY  READ 221 00:03:1812 125 125
2: POSIX  ADVISORY  READ 221 00:03:1812 128 128
3: POSIX  ADVISORY  READ 221 00:03:1809 1073741826 1073742335
4: FLOCK  ADVISORY  READ 221 00:03:1808 0 EOF
```

221 holds the big lock shared (inode 1808), SQLite's lock on the
database (1809) and a read mark in the WAL index (1812). It is
inside a read transaction, and nothing else is in its way.

## Ask gdb

`rewind shell` opens a shell in a throwaway fork of the run at a step. gdb
attaches to 221 there, stops it where it handles SQLite's next busy
answer, and catches the retry after that. The shell reads these
lines from [gdb.sh](../templates/nix-schema-migration-hang-gdb.sh); typed in, they do the same:

```console
$ rewind shell 3d0a28b5 200000 --pid 221 < gdb.sh
rewind: a shell at step 200000 of 3d0a28b5924960cf; exit it to leave
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
[rewind] /build/source/tests/functional/ca # gdb -p 221 -batch -x /tmp/g.cmd 2>&1 | grep -vE "^warning|auto-load|add-auto-load|line to your|To (enable|completely)|set auto-load|For more information|info .\(gdb\)|^$"
[New LWP 227]
...
Breakpoint 1 at 0x7fa3cea39b76
Thread 1 "nix" hit Breakpoint 1, 0x00007fa3cea39b76 in nix::handleSQLiteBusy(nix::SQLiteBusy const&, long&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#0  0x00007fa3cea39b76 in nix::handleSQLiteBusy(nix::SQLiteBusy const&, long&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#1  0x00007fa3ce8e28d5 in void nix::retrySQLite<void, nix::SQLite::exec(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&)::{lambda()#1}>(nix::SQLite::exec(std::__cxx11::basic_string< ...
#2  0x00007fa3ce9e037e in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&)::{lambda(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&, std::__cxx11::basic_string<char, std::c ...
#3  0x00007fa3ce9e28a9 in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#4  0x00007fa3ce9e3605 in nix::LocalStore::LocalStore(nix::ref<nix::LocalStoreConfig const>) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#5  0x00007fa3ce9dd046 in nix::LocalStoreConfig::openStore() const () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#6  0x00007fa3ce123ddc in nix::StoreCommand::createStore() () from /nix/store/...-nix-cmd-2.35.0pre/lib/libnixcmd.so.2.35.0
#7  0x00007fa3ce123edc in nix::StoreCommand::getStore() () from /nix/store/...-nix-cmd-2.35.0pre/lib/libnixcmd.so.2.35.0
...
Breakpoint 2 at 0x7fa3ccef9a68
Thread 1 "nix" hit Breakpoint 2, 0x00007fa3ccef9a68 in sqlite3_exec () from /nix/store/...-sqlite-3.50.4/lib/libsqlite3.so
statement: drop index if exists IndexReferrer;
insert into SchemaMigrations values('20260309-drop-redundant-indexreferrer')
...
sqlite3_exec returned 5
extended error code 517
errmsg database is locked
autocommit 0
[Inferior 1 (process 221) detached]
[rewind] /build/source/tests/functional/ca # exit
```

221 is retrying the second migration. SQLite returns `SQLITE_BUSY` (5)
with the extended code 517, `SQLITE_BUSY_SNAPSHOT`, and `autocommit 0` says
the connection is still inside the migration's transaction.

## Root cause

`SQLITE_BUSY_SNAPSHOT`
([SQLite docs](https://www.sqlite.org/rescode.html#busy_snapshot)) is what a WAL connection gets when it tries to write after another connection has
committed since its read. Its snapshot stays stale until the transaction ends.

221 began the migration and read the schema, which fixed its snapshot.
Another `nix` process committed a migration. 221's write was refused, and
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
schedule   0: exited:0            33398 steps  a50a5ab6d992  run 09ad29e03cd63ffa
...
schedule  42: exited:1           599402 steps    run 80467f8adb93bc71
...
schedule  45: exited:1           447260 steps    run a62d682efd9861cb
...
schedule  65: exited:1           448205 steps    run 4ea66f7cd357947d
...
schedule  74: exited:1           453073 steps    run 5d6332f68a5f728d
...
schedule  91: exited:1           447935 steps    run 251ba6ae151c8084
...
schedule 101: exited:1           446546 steps    run d38ae69d8c315cd3
...
schedule 119: exited:1           596798 steps    run 786445ae6b36fd83
...
schedule 141: exited:1           451235 steps    run 4c8000c7b3e240fd
schedule 142: exited:1           450899 steps    run 35d505c140b018a6
...
schedule 155: exited:1           446874 steps    run 536d8149d88b65cf
...
schedule 196: exited:1           450741 steps    run b7bdbab43729ea82
...
schedule 219: exited:1           449751 steps    run 39391a8d4ac8ec47
...
schedule 240: exited:1           450642 steps    run 7d87c15b9b0ae3ee
...
schedule 242: exited:1           445958 steps    run 1eb4b55cb33f1872
...
schedule 247: exited:1           451698 steps    run 0e294f11cee0d8a0
...
15 of 256 other schedules ended differently
...
```

```console
$ rewind log 80467f8a | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      1 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.06s   killed by signal 15 SIGTERM
      1 1/1 nix-functional-tests:ca / concurrent-builds        TIMEOUT        300.06s   killed by signal 15 SIGTERM
     56 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy
```

All 15 failing runs hang the same way, with no UNIQUE error.
[#15967](https://github.com/NixOS/nix/pull/15967) takes the big lock
exclusively while a migration runs, so no other process can commit during its
transaction. Nix 2.35.2 from nixpkgs has both fixes and passes every schedule:

```console
$ rewind check --epoch 1791936000 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds'
rewind: packing 217 store paths for nix-functional-concurrent-builds-2.35.2
schedule   0: exited:0            36371 steps  a50a5ab6d992  run 2e00d261c8f42380
...
0 of 256 other schedules ended differently
same result under all 257 schedules
```

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-schema-migration-hang.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd)
  (223.0 KB), the trace, for `rewind events`, `rewind log` and the app; the
  [viewer](https://rewindvm.dev/viewer/?url=examples/nix-schema-migration-hang.rwd) opens it in a browser.
- [nix-schema-migration-hang-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd)
  (526.5 MB), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd
$ rewind replay 3d0a28b5
$ rewind shell 3d0a28b5 <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd
```
