# Case study: a hang in Nix's store schema migration

In April 2026 a Nix master build could hang forever when several `nix`
commands opened a store at once, printing `SQLite database ... is busy` every
ten seconds. This is a known bug, reported as
[NixOS/nix#15693](https://github.com/NixOS/nix/issues/15693) and fixed in two
steps. Rewind VM reproduced it on the exact version in the report, pinned the
hang to one SQLite error code with gdb inside the VM, and showed why the first
fix did not stop the hang while the second did. None of the bug is a
discovery; the diagnosis of the hang is more specific than the one in the
issue.

## The software

`LocalStore::LocalStore` opens the store's SQLite database (in WAL mode) and
runs schema migrations. Since Nix 2.26 a migration is a named SQL script
recorded in a `SchemaMigrations` table, and on 2026-03-09 master added one
that every store runs, dropping the `IndexReferrer` index. At
[a94dee99e](https://github.com/NixOS/nix/blob/a94dee99e1805b1df24daefcdfa86a3d50c63685/src/libstore/local-store.cc#L566-L601),
the version the issue names, `upgradeDBSchema` reads the table, then runs each
missing migration in its own transaction:

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

Every process does this while holding the store's big lock in shared mode
only, so several can migrate at once. `SQLiteTxn` is a plain `begin;`, and
`SQLite::exec` retries its statement for as long as SQLite says the database
is busy
([sqlite.cc](https://github.com/NixOS/nix/blob/a94dee99e1805b1df24daefcdfa86a3d50c63685/src/libstore/sqlite.cc#L130-L136)):

```cpp
void SQLite::exec(const std::string & stmt)
{
    retrySQLite<void>([&]() {
        if (sqlite3_exec(db, stmt.c_str(), 0, 0, 0) != SQLITE_OK)
            SQLiteError::throw_(db, "executing SQLite statement '%s'", stmt);
    });
}
```

`retrySQLite` catches `SQLiteBusy`, sleeps up to 100 ms, prints a warning at
most every 10 seconds, and tries again, with no limit.

The test that shows it is
[tests/functional/ca/concurrent-builds.sh](https://github.com/NixOS/nix/blob/a94dee99e1805b1df24daefcdfa86a3d50c63685/tests/functional/ca/concurrent-builds.sh):
six `nix build` processes started at once on an empty store, with
content-addressed derivations on, so there are two migrations to run.

## Reproducing it

The derivation is Nix's own `nix-functional-tests` from its flake at
a94dee99e, which cache.nixos.org has, with the check phase cut down to
`meson test --no-rebuild concurrent-builds` and gdb added to the inputs so a
`rewind shell` can use it later. Under 256 perturbed schedules:

```console
$ rewind check --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds-15693-gdb'
schedule   0: exited:0      33456 steps  a50a5ab6d992  run e262757d97de859c
schedule   1: exited:0      44441 steps  a50a5ab6d992  run 154889d891d40ce6
...
schedule  81: exited:1     409405 steps    run d2836518b6663558
...
schedule 108: exited:1     411001 steps    run 75647cfd4e5f0397
...
schedule 137: exited:1     409510 steps    run 36345548e48f26bc
...
schedule 243: exited:1     406408 steps    run b220275a4437155e
...
schedule 256: exited:1     680682 steps    run f107b67c001069e1
5 of 256 perturbed schedules ended differently

schedule 81 ends differently; narrowing the steps it perturbs
perturbing only steps 318..25010 still ends differently
step 25009 decides it: a reschedule there makes the run fail

passing: run 080aeb48c9d945bb, schedule 81 over steps 318..25009
failing: run 2b985e2716306fa6, schedule 81 over steps 318..25010
the two are the same run until step 25009

where /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh first behaves differently:
  both          35044   188/188   SIGCHLD code=1 addr=0x0
  both          37005   188/188   SIGCHLD code=1 addr=0x0
  both          42508   188/188   SIGCHLD code=1 addr=0x0
  failing      266982   188/188   SIGTERM code=0 addr=0x0
  failing      266983   188/188   exit_group(bash) killed:SIGTERM
  passing       36524   188/188   exit_group(bash) exited:0

open both in the desktop app: rewind open 2b985e2716306fa6 266982 --compare 080aeb48c9d945bb
```

Narrowing keeps a wide window here, from early in boot to a few thousand steps
after the six `nix` processes start at step 20551: they race from the moment
they start, and the steps that matter are spread over all of them. The
window's last step, 25009, decides it: without its reschedule the same window
passes, and that run, 080aeb48, is the one `check` compares the failing run
with. The two are the same run until step 25009, 4458 steps after the `nix`
processes start. `check` names the test script as the first process to behave
differently, since in the failing run meson's SIGTERM ends it where in the
passing run it exits 0. The
same derivation without gdb in its inputs failed under 7 of 256 schedules. On
the host, the test passed 51 runs out of 51.

The failing run ends with meson's 300 second timeout, which the VM reaches in
seconds of wall time since a sleeping machine skips ahead to its next timer:

```console
$ rewind log d2836518 | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      1 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.01s   killed by signal 15 SIGTERM
      1 1/1 nix-functional-tests:ca / concurrent-builds        TIMEOUT        300.01s   killed by signal 15 SIGTERM
      1        insert into SchemaMigrations values('20251017-ca-derivations')': constraint failed, UNIQUE constraint failed: SchemaMigrations.migration (in '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite')
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy

$ rewind events d2836518 | grep -E 'exit_group\(nix\)'
     25761   225/225   exit_group(nix) exited:1
     30286   238/238   exit_group(nix) exited:0
     37077   222/222   exit_group(nix) exited:0
     39954   223/223   exit_group(nix) exited:0
     47067   224/224   exit_group(nix) exited:0
     53939   221/221   exit_group(nix) exited:0
    409218   220/220   exit_group(nix) exited:1
```

The TIMEOUT line is there twice: meson prints it as the test ends and again
in its summary. A Nix build writes to a terminal, where meson pads the first
copy, so `uniq` counts the two apart.

Both failure modes from the issue are in this one run. Process 225 lost the
race to record the `20251017-ca-derivations` migration and exited with the
UNIQUE constraint error. Process 220 never got past opening the store; it
exited only after meson killed the test at step 409218. (Process 238 is
`nix __build-remote`, the build hook. It exits 0 here, as it does in 18 of the
252 passing runs; in most of the others it ends with SIGTERM.)
The events also show all six processes writing `var/nix/db/schema`, so each
of them took the new store branch of the constructor.

## Where the hung process is

At step 200000, in the middle of the hang, only 220 is left:

```console
$ rewind ps d2836518 200000
     1 /init
    34   bash -e /nix/store/...-source-stdenv.sh /nix/store/...-default-builder.sh
   187     /nix/store/...-python3-3.13.11/bin/python3.13 /nix/store/...-meson-1.9.1/bin/meson test --no-rebuild --print-errorlogs concurrent-builds
   188       /nix/store/...-bash-5.3p3/bin/bash -x -e -u -o pipefail concurrent-builds.sh
   220         nix build --no-link --file ./racy.nix
   226           (thread)

$ rewind cat d2836518 200000 /proc/locks
1: POSIX  ADVISORY  READ 220 00:03:1812 124 124
2: POSIX  ADVISORY  READ 220 00:03:1812 128 128
3: POSIX  ADVISORY  READ 220 00:03:1809 1073741826 1073742335
4: FLOCK  ADVISORY  READ 220 00:03:1808 0 EOF
```

No other process holds a lock. 220 holds the big lock shared (inode 1808),
SQLite's shared lock on the database (1809), and in the WAL index (1812) the
read lock at offset 124, which is SQLite's read mark 1. That read lock means
220's connection is inside a read transaction, and nothing else is in its way.

`rewind shell` opens a shell in a throwaway fork of the run at a step, with
everything else stopped. gdb is in the closure, so it can attach to 220 there
and let it run until the next retry. The inspection leaves a SIGSTOP pending
on every thread, which gdb has to be told to swallow:

```console
$ rewind shell d2836518 200000 --pid 220
rewind: a shell at step 200000 of d2836518b6663558; exit it to leave
[rewind] /build/source/tests/functional/ca # cat /tmp/g.cmd
set pagination off
handle SIGSTOP nostop noprint nopass
set unwind-on-signal on
bt
break sqlite3_exec
continue
set $db = $rdi
printf "statement: %.200s\n", (char *) $rsi
finish
printf "sqlite3_exec returned %d\n", (int) $rax
printf "extended error code %d\n", (int) sqlite3_extended_errcode($db)
printf "errmsg %s\n", (char *) sqlite3_errmsg($db)
printf "autocommit %d\n", (int) sqlite3_get_autocommit($db)
detach
[rewind] /build/source/tests/functional/ca # gdb -p 220 -batch -x /tmp/g.cmd 2>&1 | grep -vE "^warning|auto-load|add-auto-load|line to your|To (enable|completely)|set auto-load|For more information|info .\(gdb\)|^$"
[New LWP 226]
...
#0  0x00007fbfbf826223 in clock_nanosleep@GLIBC_2.2.5 () from /nix/store/...-glibc-2.40-218/lib/libc.so.6
#1  0x00007fbfbf8328e7 in nanosleep () from /nix/store/...-glibc-2.40-218/lib/libc.so.6
#2  0x00007fbfc057dc1a in nix::handleSQLiteBusy(nix::SQLiteBusy const&, long&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#3  0x00007fbfc04268d5 in void nix::retrySQLite<void, nix::SQLite::exec(...)::{lambda()#1}>(...) ...
#4  0x00007fbfc052437e in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&)::{lambda(...)#1}::operator()(...) ...
#5  0x00007fbfc05268a9 in nix::LocalStore::upgradeDBSchema(nix::LocalStore::State&) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#6  0x00007fbfc0527605 in nix::LocalStore::LocalStore(nix::ref<nix::LocalStoreConfig const>) () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
#7  0x00007fbfc0521046 in nix::LocalStoreConfig::openStore() const () from /nix/store/...-nix-store-2.35.0pre/lib/libnixstore.so.2.35.0
...
Breakpoint 1 at 0x7fbfbea3da68
Thread 1 "nix" hit Breakpoint 1, 0x00007fbfbea3da68 in sqlite3_exec () from /nix/store/...-sqlite-3.50.4/lib/libsqlite3.so
statement: drop index if exists IndexReferrer;
insert into SchemaMigrations values('20260309-drop-redundant-indexreferrer')
...
sqlite3_exec returned 5
extended error code 517
errmsg database is locked
autocommit 0
[Inferior 1 (process 220) detached]
```

The command file was written with a heredoc earlier in the same session, cut
here. 220 is in `doUpgrade`, retrying the second migration. SQLite returns
`SQLITE_BUSY` (5) with the extended code 517, `SQLITE_BUSY_SNAPSHOT`, and
`autocommit 0` says the connection is inside the transaction that `SQLiteTxn`
began.

## Root cause

[SQLite's result code documentation](https://www.sqlite.org/rescode.html#busy_snapshot)
describes `SQLITE_BUSY_SNAPSHOT` as what a WAL connection gets when it tries
to turn a read transaction into a write transaction after another connection
has already written to the database. The connection's view of the database
is obsolete, and it stays obsolete until the transaction ends.

So the hang is this interleaving. 220 begins the migration's transaction and
reads (the `drop index if exists` reads the schema), which fixes its
snapshot. Another `nix` process commits a migration. 220's write is refused
with `SQLITE_BUSY_SNAPSHOT`. Nix maps every `SQLITE_BUSY` to `SQLiteBusy`, and
`SQLite::exec` retries the statement inside the same open transaction, on the
same stale snapshot, which SQLite refuses every time. The retry loop has no
limit: 28 warnings, ten seconds apart, until meson kills the test.

The UNIQUE failure is the other outcome of the same race: a process that
reads `SchemaMigrations` before another commits, and writes after it, with a
snapshot that is still current, runs the migration again and fails on the
insert.

## The two fixes, checked

[NixOS/nix#15694](https://github.com/NixOS/nix/pull/15694), merged on
2026-04-16, changed the insert to `insert or ignore`. That removes the UNIQUE
error but not the stale snapshot. The issue stayed open with the comment "Not
fixed, we are still observing this issue." The same check at the merge commit
c390460cd:

```console
$ rewind check --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds-15694'
schedule   0: exited:0      36351 steps  a50a5ab6d992  run 3237e6cda5ec1904
...
schedule   8: exited:1     414562 steps    run c42a767edfa279f2
...
schedule 212: exited:1     551096 steps    run eb6069df96f25b09
...
10 of 256 perturbed schedules ended differently

$ rewind log c42a767e | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
      1 1/1 nix-functional-tests:ca / concurrent-builds TIMEOUT        300.01s   killed by signal 15 SIGTERM
      1 1/1 nix-functional-tests:ca / concurrent-builds        TIMEOUT        300.01s   killed by signal 15 SIGTERM
     28 warning: SQLite database '/build/nix-test/ca/concurrent-builds/var/nix/db/db.sqlite' is busy
```

None of the ten failing runs has a UNIQUE error. Nine of them hang one `nix`
process the same way, each with 28 warnings; under schedule 212 two hang,
with 56.
[NixOS/nix#15967](https://github.com/NixOS/nix/pull/15967), merged on
2026-06-08, checks whether a migration is needed while holding the big lock
shared, and if one is, takes the lock exclusively to run it, then drops back
to shared and keeps holding it for the life of the store. With that, no other
Nix process can commit while a migration's transaction is open, so its
snapshot cannot go stale. Nix 2.35.2 has both fixes, and the same test from
nixpkgs' build of it passed every schedule:

```console
$ rewind check --all --schedules 256 'github:fzakaria/rewindvm?dir=examples/case-studies#nix-concurrent-builds'
schedule   0: exited:0      34833 steps  a50a5ab6d992  run 41515d08c46681f5
...
0 of 256 perturbed schedules ended differently
same result under all 257 schedules
```

`SQLite::exec` still retries inside whatever transaction it is called in. On
master the migrations are its only callers inside a `SQLiteTxn` in
`local-store.cc`, and they now run under the exclusive lock.

## Making it fail more often

Rewind's schedules now include stalls: at one exit in 128 the running task
sleeps for 10 µs to 1.28 ms when it next returns to user space. The rates
above were measured with them. With the previous version of Rewind, which only
reordered, the same derivation at a94dee99e failed under 3 of 256 schedules
and the first fix under 2 of 256. With stalls they fail under 5 and 10 of
256: a little more often for the reported bug, and five times as often for
the first fix. The guest kernel also changed between the two measurements, to
give a Nix build a terminal, so not all of the difference is the stalls'.

Earlier, to get more failing runs to study, an `LD_PRELOAD` library that
sleeps up to 5 ms at one call in 16 to `fcntl`, `flock`, `rename`, `unlink`,
`connect`, `pread64`, `pwrite64` and `fsync`, chosen by a hash of a seed, the
thread id and the call count, was preloaded into the test, under the previous
version of Rewind without stalls. It is not part of Rewind. With it:

| Nix                         | Schedules that hang |
| --------------------------- | ------------------- |
| a94dee99e, the reported bug | 15 of 65            |
| c390460cd, the first fix    | 16 of 65            |
| 2.35.2, both fixes          | 0 of 65             |

The shim's stalls are longer and fall on the calls around SQLite's locks,
which is where this race lives; Rewind's land on any exit.

## The recording

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies). `rewind import` and the
desktop app (`rewind-app`) take either one, by path or URL, and unpack it as
it downloads:

- [nix-schema-migration-hang.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd)
  (42.9 KB) is the trace alone, enough for `rewind events`, `rewind log` and
  the app.
- [nix-schema-migration-hang-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd)
  (544.5 MB) adds the kernel, the input image and the keyframes, so
  another AMD machine from Zen 2 on can `rewind replay` and `rewind shell` it.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd
$ rewind replay d2836518
$ rewind shell d2836518 <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd
```

How they were made:

```console
$ rewind replay d2836518
identical: 3483 events over 409405 steps
$ rewind export d2836518 --replayable -o nix-schema-migration-hang-replayable.rwd
rewind: wrote nix-schema-migration-hang-replayable.rwd (544.5 MB)
$ rewind export d2836518 -o nix-schema-migration-hang.rwd
rewind: wrote nix-schema-migration-hang.rwd (42.9 KB)
```
