# Case study: a hang in Nix's store schema migration

<!-- let epoch = 1791936000 -->
<!-- let flake = github:fzakaria/rewindvm?dir=examples/case-studies -->
<!-- let schedule = 31 -->

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

```console run name=check elide keep=^schedule\s+[01]:|^schedule\s+\d+:\sexited:1\s|^(?!schedule\s+\d+:)
$ rewind check --epoch {{epoch}} --all --schedules 256 '{{flake}}#nix-concurrent-builds-15693-gdb'
```

<!-- set nfails: grep -cE '^schedule +[0-9]+: exited:1 ' {{out:check}} -->
<!-- assert: grep -qE '^schedule +0: exited:0 ' {{out:check}} && grep -qE '^{{nfails}} of 256 other schedules ended differently$' {{out:check}} -->
<!-- set run: grep -E '^schedule +{{schedule}}: exited:1 ' {{out:check}} | awk '{print $NF}' -->

Schedule 0 passes and {{nfails}} of the other 256 fail. The narrowed window
stays wide because the six `nix` processes race from the moment they start. On the host the test passed 51 of
51 runs.

## What the failing run shows

<!-- set nix_exits: rewind events {{run}} | grep -E 'exit_group\(nix\)' -->
<!-- set hung: rewind events {{run}} | grep -E 'exit_group\(nix\) exited:1$' | tail -1 | awk '{print $2}' | cut -d/ -f1 -->
<!-- set hung_exit: rewind events {{run}} | grep -E 'exit_group\(nix\) exited:1$' | tail -1 | awk '{print $1}' -->
<!-- set losers: rewind events {{run}} | grep -E 'exit_group\(nix\) exited:1$' | head -n -1 | awk '{print $2}' | cut -d/ -f1 | tr '\n' ' ' -->
<!-- set hook: rewind events {{run}} | grep -E 'exit_group\(nix\) killed:SIGTERM$' | awk '{print $2}' | cut -d/ -f1 -->

Schedule {{schedule}} ends with meson's 300 second timeout, reached in seconds
of wall time since the VM skips ahead while it sleeps:

```console run name=failing
$ rewind log {{run|short}} | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c

$ rewind events {{run|short}} | grep -E 'exit_group\(nix\)'
```

<!-- set unique: grep -E 'UNIQUE constraint failed' {{out:failing}} | awk '{s += $1} END {print s}' -->
<!-- set busy: grep -E ' is busy$' {{out:failing}} | awk '{print $1}' -->
<!-- assert: test {{unique}} -eq 2 && test $(echo {{losers}} | wc -w) -eq 2 && grep -q 'concurrent-builds TIMEOUT .* killed by signal 15 SIGTERM' {{out:failing}} -->
<!-- assert: test $(grep -c 'exit_group(nix) exited:1$' {{out:failing}}) -eq 3 && test $(grep -c 'exit_group(nix) killed:SIGTERM$' {{out:failing}}) -eq 1 && test $(grep -c 'exit_group(nix) exited:0$' {{out:failing}}) -eq 3 -->

Both failures from the issue are in this run. {{losers|and}} lost the race to
record a migration and exited with the UNIQUE error. {{hung}} hung until meson
killed the test at step {{hung_exit}}. ({{hook}} is the build hook, which often
ends with SIGTERM in passing runs too.)

## Where the hung process is

<!-- set shell_step: rewind threads {{run}} --from 200000 --to 260000 | awk '$3 == "idle" {print $1; exit}' -->

At step {{shell_step}}, with the VM idle while {{hung}} sleeps between two
retries, only {{hung}} is left, and it holds every lock:

```console run name=locks elide
$ rewind ps {{run|short}} {{shell_step}}

$ rewind cat {{run|short}} {{shell_step}} /proc/locks
```

<!-- assert: test $(grep -cE '^ +[0-9]+ +nix ' {{out:locks}}) -eq 1 && grep -qE '^ +{{hung}} +nix build --no-link --file ./racy.nix$' {{out:locks}} -->
<!-- assert: test $(grep -cE '^[0-9]+: ' {{out:locks}}) -eq 4 && test $(grep -E '^[0-9]+: ' {{out:locks}} | grep -cE ' READ {{hung}} ') -eq 4 -->
<!-- set big_lock: grep -E '^[0-9]+: FLOCK +ADVISORY +READ {{hung}} ' {{out:locks}} | awk '{print $6}' | cut -d: -f3 -->
<!-- set db_lock: grep -E '^[0-9]+: POSIX +ADVISORY +READ {{hung}} .* 1073741826 1073742335$' {{out:locks}} | awk '{print $6}' | cut -d: -f3 -->
<!-- set wal_lock: grep -E '^[0-9]+: POSIX +ADVISORY +READ {{hung}} .* 12[0-9] 12[0-9]$' {{out:locks}} | awk '{print $6}' | cut -d: -f3 | sort -u -->

{{hung}} holds the big lock shared (inode {{big_lock}}), SQLite's lock on the
database ({{db_lock}}) and a read mark in the WAL index ({{wal_lock}}). It is
inside a read transaction, and nothing else is in its way.

## Ask gdb

<!-- run: sed 's/@PID@/{{hung}}/' {{dir}}/nix-schema-migration-hang-gdb.sh > gdb.sh -->

`rewind shell` opens a shell in a throwaway fork of the run at a step. gdb
attaches to {{hung}} there, stops it where it handles SQLite's next busy
answer, and catches the retry after that. The shell reads these
lines from [gdb.sh](../templates/nix-schema-migration-hang-gdb.sh); typed in, they do the same:

```console run name=gdb elide cut=220 keep=^rewind:|^\x5brewind]|^>|^\x5bNew|^#[0-7]\s|^Breakpoint|^Thread|^statement|^insert|^sqlite3_exec|^extended|^errmsg|^autocommit|^\x5bInferior
$ rewind shell {{run|short}} {{shell_step}} --pid {{hung}} < gdb.sh
```

<!-- assert: grep -q '^#0 .*nix::handleSQLiteBusy' {{out:gdb}} && grep -q 'in nix::LocalStore::upgradeDBSchema' {{out:gdb}} -->
<!-- assert: grep -q "^insert into SchemaMigrations values('20260309-drop-redundant-indexreferrer')$" {{out:gdb}} -->
<!-- assert: grep -q '^sqlite3_exec returned 5$' {{out:gdb}} && grep -q '^extended error code 517$' {{out:gdb}} && grep -q '^autocommit 0$' {{out:gdb}} -->

{{hung}} is retrying the second migration. SQLite returns `SQLITE_BUSY` (5)
with the extended code 517, `SQLITE_BUSY_SNAPSHOT`, and `autocommit 0` says
the connection is still inside the migration's transaction.

## Root cause

`SQLITE_BUSY_SNAPSHOT`
([SQLite docs](https://www.sqlite.org/rescode.html#busy_snapshot)) is what a WAL connection gets when it tries to write after another connection has
committed since its read. Its snapshot stays stale until the transaction ends.

{{hung}} began the migration and read the schema, which fixed its snapshot.
Another `nix` process committed a migration. {{hung}}'s write was refused, and
`SQLite::exec` retried the statement inside the same transaction, on the same
stale snapshot, forever: {{busy}} warnings, ten seconds apart. A process whose
read came before the other commit but whose snapshot was still current ran
the migration again and failed with the UNIQUE error instead.

## Check the two fixes

[#15694](https://github.com/NixOS/nix/pull/15694) changed the insert to
`insert or ignore`. That removes the UNIQUE error, but not the stale snapshot.
At its merge commit
[c390460cd](https://github.com/NixOS/nix/commit/c390460cdf7ee8b3208d982e09f91555f980759e):

```console run name=first_fix keep=^rewind:|^schedule\s+0:|^schedule\s+\d+:\sexited:1\s|\sof\s256\sother
$ rewind check --epoch {{epoch}} --all --schedules 256 '{{flake}}#nix-concurrent-builds-15694'
```

<!-- set first_fix_fails: grep -cE '^schedule +[0-9]+: exited:1 ' {{out:first_fix}} -->
<!-- set first_fix_run: grep -m1 -E '^schedule +[0-9]+: exited:1 ' {{out:first_fix}} | awk '{print $NF}' -->
<!-- assert: grep -qE '^schedule +0: exited:0 ' {{out:first_fix}} && test {{first_fix_fails}} -gt 0 -->
<!-- assert: for r in $(grep -E '^schedule +[0-9]+: exited:1 ' {{out:first_fix}} | awk '{print $NF}'); do log=$(rewind log $r | sed 's/\x1b\[[0-9;]*m//g'); grep -q 'concurrent-builds TIMEOUT' <<< "$log" || exit 1; ! grep -q UNIQUE <<< "$log" || exit 1; done -->

```console run name=first_fix_log
$ rewind log {{first_fix_run|short}} | grep -E 'UNIQUE|is busy|TIMEOUT' | sort | uniq -c
```

All {{first_fix_fails}} failing runs hang the same way, with no UNIQUE error.
[#15967](https://github.com/NixOS/nix/pull/15967) takes the big lock
exclusively while a migration runs, so no other process can commit during its
transaction. Nix 2.35.2 from nixpkgs has both fixes and passes every schedule:

```console run name=released keep=^rewind:|^schedule\s+0:|^0\sof\s256|^same\sresult
$ rewind check --epoch {{epoch}} --all --schedules 256 '{{flake}}#nix-concurrent-builds'
```

<!-- assert: grep -q '^same result under all 257 schedules$' {{out:released}} -->

## The recording

<!-- set trace_size: rewind export {{run}} -o {{work}}/nix-schema-migration-hang.rwd 2>&1 | grep -o '([^)]*)' | tr -d '()' -->
<!-- set replayable_size: rewind export {{run}} --replayable -o {{work}}/nix-schema-migration-hang-replayable.rwd 2>&1 | grep -o '([^)]*)' | tr -d '()' -->
<!-- run: cp {{work}}/nix-schema-migration-hang.rwd {{site}}/viewer/examples/nix-schema-migration-hang.rwd -->

Both files are in the
[case-studies release](https://github.com/fzakaria/rewindvm/releases/tag/case-studies):

- [nix-schema-migration-hang.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd)
  ({{trace_size}}), the trace, for `rewind events`, `rewind log` and the app; the
  [viewer](https://rewindvm.dev/viewer/?url=examples/nix-schema-migration-hang.rwd) opens it in a browser.
- [nix-schema-migration-hang-replayable.rwd](https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd)
  ({{replayable_size}}), with the kernel, input image and keyframes, for `rewind replay`
  and `rewind shell` on an AMD Zen 2 or later.

```console
$ rewind import https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang-replayable.rwd
$ rewind replay {{run|short}}
$ rewind shell {{run|short}} <step>
$ rewind-app https://github.com/fzakaria/rewindvm/releases/download/case-studies/nix-schema-migration-hang.rwd
```
