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
schedule   0: exited:0            30956 steps  a50a5ab6d992  run 5a8dd4a66c56e643
schedule   1: exited:0            43466 steps  a50a5ab6d992  run 4d3c638c841173b5
...
schedule   5: exited:1           423262 steps    run d7f79e135fd74035
...
schedule  31: exited:1           425496 steps    run fa92b5828ce2ca6e
...
schedule  47: exited:1           424122 steps    run e10e618bd33f0c38
...
schedule  57: exited:1           426677 steps    run 94b25e8ae4025bbe
...
schedule  68: exited:1           422537 steps    run e4e7df6ada8e59d2
...
schedule 126: exited:1           424495 steps    run f17ca59f52fb715c
...
schedule 136: exited:1           427329 steps    run 2aac0dbc2cae9117
...
schedule 146: exited:1           574750 steps    run 3bce3f1ebf4180c0
...
schedule 161: exited:1           573380 steps    run effd676fff878f21
...
schedule 166: exited:1           575589 steps    run 7e36434f4f756474
...
schedule 216: exited:1           427275 steps    run c269399913e017b1
...
schedule 229: exited:1           425123 steps    run 858e355550251314
...
schedule 243: exited:1           424836 steps    run 01f07f12931e8b6f
...
schedule 249: exited:1           422889 steps    run 2672e5c4c6bb591b
...
14 of 256 other schedules ended differently

schedule 5 ends differently from schedule 0; finding the step that decides it
schedule 5 limited to steps 3651..23900 still ends differently
step 23899 decides it: a request to switch threads there makes the run fail

passing: run 81a2df9a933f2b5f, schedule 5 over steps 3651..23899
failing: run 97c7a128b940042a, schedule 5 over steps 3651..23900
the two are the same run until step 23899

where /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh first behaves differently:
  both          20467   188/188   fork() = 224
  both          20507   188/188   fork() = 225
  both          35666   188/188   SIGCHLD code=1 addr=0x0
  failing      277383   188/188   SIGTERM code=0 addr=0x0
  failing      277384   188/188   exit_group(bash) killed:SIGTERM
  passing       33447   188/188   SIGCHLD code=1 addr=0x0
  passing       33448   188/188   exit_group(bash) exited:0

open both in the desktop app: rewind open 97c7a128b940042a 277383 --compare 81a2df9a933f2b5f
```

Schedule 0 passes and 14 of the other 256 fail. The narrowed window
stays wide because the six `nix` processes race from the moment they start. On the host the test passed 51 of
51 runs.

## What the failing run shows

Schedule 31 ends with meson's 300 second timeout, reached in seconds
of wall time since the VM skips ahead while it sleeps:

```console
$ rewind log fa92b582 | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      1 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.01s   killed by signal 15 SIGTERM
      1 1/1 nix-functional-tests:ca / concurrent-builds        TIMEOUT        300.01s   killed by signal 15 SIGTERM
      1        insert into SchemaMigrations values('20251017-ca-derivations')': constraint failed, UNIQUE constraint failed: SchemaMigrations.migration (in '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite')
      1        insert into SchemaMigrations values('20260309-drop-redundant-indexreferrer')': constraint failed, UNIQUE constraint failed: SchemaMigrations.migration (in '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite')
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy

$ rewind events fa92b582 | grep -E 'exit_group\(nix\)'
     26648   221/221   exit_group(nix) exited:1
     27577   222/222   exit_group(nix) exited:1
     31642   238/238   exit_group(nix) killed:SIGTERM
     36376   224/224   exit_group(nix) exited:0
     39516   220/220   exit_group(nix) exited:0
     46690   225/225   exit_group(nix) exited:0
    425304   223/223   exit_group(nix) exited:1
```

Both failures from the issue are in this run. 221 and 222 lost the race to
record a migration and exited with the UNIQUE error. 223 hung until meson
killed the test at step 425304. (238 is the build hook, which often
ends with SIGTERM in passing runs too.)

## Where the hung process is

At step 200000, with the VM idle while 223 sleeps between two
retries, only 223 is left, and it holds every lock:

```console
$ rewind ps fa92b582 200000
     1 /init
    34   bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   187     /nix/store/...-python3-3.13.11/bin/python3.13 /nix/store/...-meson-1.9.1/bin/meson test --no-rebuild --print-errorlogs concurrent-builds
   188       /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh
   223         nix build --no-link --file ./racy.nix
   228           (thread)

$ rewind cat fa92b582 200000 /proc/locks
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
$ rewind shell fa92b582 200000 --pid 223 < gdb.sh
rewind: a shell at step 200000 of fa92b5828ce2ca6e; exit it to leave
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
[New LWP 228]
...
Breakpoint 1 at 0x7f5f3325ab76
Thread 1 "nix" hit Breakpoint 1, 0x00007f5f3325ab76 in nix::handleSQLiteBusy(nix::SQLiteBusy const&, long&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#0  0x00007f5f3325ab76 in nix::handleSQLiteBusy(nix::SQLiteBusy const&, long&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#1  0x00007f5f331038d5 in void nix::retrySQLite<void, nix::SQLite::exec(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&)::{lambda()#1}>(nix::SQLite::exec(std::__cxx11::basic_string< ...
#2  0x00007f5f3320137e in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&)::{lambda(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&, std::__cxx11::basic_string<char, std::c ...
#3  0x00007f5f332038a9 in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#4  0x00007f5f33204605 in nix::LocalStore::LocalStore(nix::ref<nix::LocalStoreConfig const>) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#5  0x00007f5f331fe046 in nix::LocalStoreConfig::openStore() const () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#6  0x00007f5f32944ddc in nix::StoreCommand::createStore() () from /nix/store/...-nix-cmd-2.35.0pre/lib/libnixcmd.so.2.35.0
#7  0x00007f5f32944edc in nix::StoreCommand::getStore() () from /nix/store/...-nix-cmd-2.35.0pre/lib/libnixcmd.so.2.35.0
...
Breakpoint 2 at 0x7f5f3171aa68
Thread 1 "nix" hit Breakpoint 2, 0x00007f5f3171aa68 in sqlite3_exec () from /nix/store/...-sqlite-3.50.4/lib/libsqlite3.so
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
schedule   0: exited:0            33246 steps  a50a5ab6d992  run fc62a9dc9b39d2c3
...
schedule   3: exited:1           431955 steps    run d2a4072880349ad3
...
schedule  47: exited:1           579535 steps    run 711839b954d23b23
...
schedule  80: exited:1           724396 steps    run d7a8589a4ea05578
...
schedule  93: exited:1           426173 steps    run 4782f1ed670125f9
...
schedule 113: exited:1           429523 steps    run 5a7adcb6b35c7928
...
schedule 118: exited:1           428800 steps    run e65ee45ac93fcc13
...
schedule 137: exited:1           430517 steps    run e00c5d9f710bef77
...
schedule 145: exited:1           426330 steps    run af9c9911012cbbda
...
schedule 162: exited:1           430509 steps    run 76456b619075c8ad
...
schedule 210: exited:1           426631 steps    run 8d3fdf0d05ba27df
...
schedule 226: exited:1           427936 steps    run e0019b5d94502b7b
...
schedule 242: exited:1           426209 steps    run 5890c7617e0a839f
...
schedule 248: exited:1           427501 steps    run 988c69a0bf2fee41
...
schedule 251: exited:1           426248 steps    run 6811fbfd4a5f8b4c
...
14 of 256 other schedules ended differently
...
```

```console
$ rewind log d2a40728 | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      1 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.00s   killed by signal 15 SIGTERM
      1 1/1 nix-functional-tests:ca / concurrent-builds        TIMEOUT        300.00s   killed by signal 15 SIGTERM
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy
```

All 14 failing runs hang the same way, with no UNIQUE error.
[#15967](https://github.com/NixOS/nix/pull/15967) takes the big lock
exclusively while a migration runs, so no other process can commit during its
transaction. Nix 2.35.2 from nixpkgs has both fixes and passes every schedule:

```console
$ rewind check --epoch 1791936000 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds'
rewind: packing 217 store paths for nix-functional-concurrent-builds-2.35.2
schedule   0: exited:0            36781 steps  a50a5ab6d992  run 90f1add102a5bf28
...
0 of 256 other schedules ended differently
same result under all 257 schedules
```

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-schema-migration-hang.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd)
  (136.9 KB), the trace, for `rewind events`, `rewind log` and the app; the
  [viewer](https://rewindvm.dev/viewer/?url=examples/nix-schema-migration-hang.rwd) opens it in a browser.
- [nix-schema-migration-hang-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd)
  (543.0 MB), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd
$ rewind replay fa92b582
$ rewind shell fa92b582 <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd
```
