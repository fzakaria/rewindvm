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
schedule   0: exited:0            26560 steps  a50a5ab6d992  run b6baadbf1198a1c7
schedule   1: exited:0            42024 steps  a50a5ab6d992  run 7ac0fe3453285879
...
schedule  52: exited:1           445990 steps    run 5f76c37b16b58823
...
schedule  57: exited:1           448096 steps    run 6fc65a2f75014fb4
...
schedule  73: exited:1           601124 steps    run c3746ecaeeb34ecc
...
schedule  75: exited:1           444701 steps    run ddbcdc0d76409cc7
...
schedule  77: exited:1           443822 steps    run 20d41e9135124d63
...
schedule  92: exited:1           748158 steps    run 161a1aeacd6ca655
...
schedule  96: exited:1           445681 steps    run 5ae05262df69fa45
...
schedule 143: exited:1           891020 steps    run 0ae54c5bfc41aa86
...
schedule 155: exited:1           739783 steps    run 2376f25c61307e0f
...
schedule 178: exited:1           445897 steps    run 14a81184133767c5
...
schedule 190: exited:1           444658 steps    run ed43966bf7c8f225
...
schedule 201: exited:1           442519 steps    run b87f94b50ec6571d
...
schedule 244: exited:1           443200 steps    run ea261468a2d8ee42
...
13 of 256 other schedules ended differently

schedule 52 ends differently from schedule 0; finding the step that decides it
schedule 52 limited to steps 714..25778 still ends differently
step 25777 decides it: a request to switch threads there makes the run fail

passing: run b5c68f20481d2241, schedule 52 over steps 714..25777
failing: run e2c119e0105f4288, schedule 52 over steps 714..25778
the two are the same run until step 25777

where /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh first behaves differently:
  both          21911   188/188   fork() = 224
  both          21914   188/188   fork() = 225
  both          38711   188/188   SIGCHLD code=1 addr=0x0
  failing      271763   188/188   SIGTERM code=0 addr=0x0
  failing      271764   188/188   exit_group(bash) killed:SIGTERM
  passing       33799   188/188   SIGCHLD code=1 addr=0x0
  passing       35273   188/188   SIGCHLD code=1 addr=0x0
  passing       35726   188/188   SIGCHLD code=1 addr=0x0
  passing       35727   188/188   exit_group(bash) exited:0

open both in the desktop app: rewind open e2c119e0105f4288 271763 --compare b5c68f20481d2241
```

Schedule 0 passes and 13 of the other 256 fail. The step that decides
schedule 52, 25777, falls inside the race, after the test
starts its first `nix build` at step 21844: one `nix` process is
opening the store's schema file there, and a single switch at that moment
decides whether all six get through the migrations. On the host the test
passed 51 of 51 runs.

## What the failing run shows

Schedule 52 ends with meson's 300 second timeout, reached in seconds
of wall time since the VM skips ahead while it sleeps:

```console
$ rewind log 5f76c37b | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      1 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.03s   killed by signal 15 SIGTERM
      1 1/1 nix-functional-tests:ca / concurrent-builds        TIMEOUT        300.03s   killed by signal 15 SIGTERM
      2        insert into SchemaMigrations values('20251017-ca-derivations')': constraint failed, UNIQUE constraint failed: SchemaMigrations.migration (in '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite')
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy

$ rewind events 5f76c37b | grep -E 'exit_group\(nix\)'
     27219   225/235   exit_group(nix) exited:1
     27527   220/229   exit_group(nix) exited:1
     32644   238/238   exit_group(nix) killed:SIGTERM
     37582   222/228   exit_group(nix) exited:0
     40829   224/230   exit_group(nix) exited:0
     48451   223/227   exit_group(nix) exited:0
    445822   221/226   exit_group(nix) exited:1
```

Both failures from the issue are in this run. 225 and 220 lost the race to
record a migration and exited with the UNIQUE error. 221 hung until meson
killed the test at step 445822. (238 is the build hook, which often
ends with SIGTERM in passing runs too.)

## Where the hung process is

At step 200000, with the VM idle while 221 sleeps between two
retries, only 221 is left, and it holds every lock:

```console
$ rewind ps 5f76c37b 200000
     1 /init
    34   bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   187     /nix/store/...-python3-3.13.11/bin/python3.13 /nix/store/...-meson-1.9.1/bin/meson test --no-rebuild --print-errorlogs concurrent-builds
   188       /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh
   221         nix build --no-link --file ./racy.nix
   226           (thread)

$ rewind cat 5f76c37b 200000 /proc/locks
1: POSIX  ADVISORY  READ 221 00:03:1812 124 124
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
$ rewind shell 5f76c37b 200000 --pid 221 < gdb.sh
rewind: a shell at step 200000 of 5f76c37b16b58823; exit it to leave
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
[New LWP 226]
...
Breakpoint 1 at 0x7fa1a6f54b76
Thread 1 "nix" hit Breakpoint 1, 0x00007fa1a6f54b76 in nix::handleSQLiteBusy(nix::SQLiteBusy const&, long&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#0  0x00007fa1a6f54b76 in nix::handleSQLiteBusy(nix::SQLiteBusy const&, long&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#1  0x00007fa1a6dfd8d5 in void nix::retrySQLite<void, nix::SQLite::exec(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&)::{lambda()#1}>(nix::SQLite::exec(std::__cxx11::basic_string< ...
#2  0x00007fa1a6efb37e in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&)::{lambda(std::__cxx11::basic_string<char, std::char_traits<char>, std::allocator<char> > const&, std::__cxx11::basic_string<char, std::c ...
#3  0x00007fa1a6efd8a9 in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#4  0x00007fa1a6efe605 in nix::LocalStore::LocalStore(nix::ref<nix::LocalStoreConfig const>) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#5  0x00007fa1a6ef8046 in nix::LocalStoreConfig::openStore() const () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#6  0x00007fa1a663eddc in nix::StoreCommand::createStore() () from /nix/store/...-nix-cmd-2.35.0pre/lib/libnixcmd.so.2.35.0
#7  0x00007fa1a663eedc in nix::StoreCommand::getStore() () from /nix/store/...-nix-cmd-2.35.0pre/lib/libnixcmd.so.2.35.0
...
Breakpoint 2 at 0x7fa1a5414a68
Thread 1 "nix" hit Breakpoint 2, 0x00007fa1a5414a68 in sqlite3_exec () from /nix/store/...-sqlite-3.50.4/lib/libsqlite3.so
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
schedule   0: exited:0            32929 steps  a50a5ab6d992  run ab8e2e874b5c269b
...
schedule   8: exited:1           449926 steps    run f8d0cb446187b035
schedule   9: exited:1           448085 steps    run 728321a798015c77
...
schedule  19: exited:1           600044 steps    run 4699976d24e8089d
...
schedule  28: exited:1           449425 steps    run 2811ec4b6aef294c
...
schedule  32: exited:1           591793 steps    run 6f7704c7acef6acc
...
schedule  76: exited:1           448145 steps    run 804edb31e27aa6b0
...
schedule  88: exited:1           596061 steps    run 42ba241e0a486a69
...
schedule  96: exited:1           445942 steps    run 48fe4791816f4676
...
schedule 122: exited:1           599241 steps    run b97eab08c2c78e15
...
schedule 135: exited:1           448365 steps    run f857754e89598da1
...
schedule 154: exited:1           448956 steps    run 089c86bea120eb3f
...
schedule 179: exited:1           448508 steps    run c0a150cead46dc67
...
schedule 183: exited:1           449140 steps    run ce4d18a825cfa7d2
...
schedule 195: exited:1           447964 steps    run eb28949a61395d4e
...
schedule 209: exited:1           601538 steps    run 7f4d8153a2c113ce
...
schedule 216: exited:1           450331 steps    run 49d19671b008d689
...
schedule 230: exited:1           596621 steps    run 5165773262d15357
...
schedule 241: exited:1           447468 steps    run 5b8d6132c2455e9e
...
18 of 256 other schedules ended differently
...
```

```console
$ rewind log f8d0cb44 | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      1 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.05s   killed by signal 15 SIGTERM
      1 1/1 nix-functional-tests:ca / concurrent-builds        TIMEOUT        300.05s   killed by signal 15 SIGTERM
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy
```

All 18 failing runs hang the same way, with no UNIQUE error.
[#15967](https://github.com/NixOS/nix/pull/15967) takes the big lock
exclusively while a migration runs, so no other process can commit during its
transaction. Nix 2.35.2 from nixpkgs has both fixes and passes every schedule:

```console
$ rewind check --epoch 1791936000 --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds'
rewind: packing 217 store paths for nix-functional-concurrent-builds-2.35.2
schedule   0: exited:0            36998 steps  a50a5ab6d992  run 1463aba2c15a8ff1
...
0 of 256 other schedules ended differently
same result under all 257 schedules
```

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-schema-migration-hang.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd)
  (199.9 KB), the trace, for `rewind events`, `rewind log` and the app; the
  [viewer](https://rewindvm.dev/viewer/?url=examples/nix-schema-migration-hang.rwd) opens it in a browser.
- [nix-schema-migration-hang-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd)
  (436.2 MB), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd
$ rewind replay 5f76c37b
$ rewind shell 5f76c37b <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd
```
