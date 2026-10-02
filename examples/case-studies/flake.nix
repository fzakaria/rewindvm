{
  description = "Rewind VM case studies: the derivations docs/case-studies runs, and the candidates tried on the way";

  # The same nixpkgs as the rewind flake pins, so everything comes from the cache.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/b4fd65b198c599cbe814fcb9f42d25d021595ec9";

  # Nix master just before and at the merge of NixOS/nix#16088, which fixed
  # a race in the Git filesystem object sink. Hydra built both, so they come
  # from cache.nixos.org.
  inputs.nix-before-16088.url = "github:NixOS/nix/fd941f6261ea423d52164c8c253add1a0551d10f";
  # Nix master on 2026-04-14, the version NixOS/nix#15693 reports
  # ca/concurrent-builds.sh hanging with: a schema migration race.
  inputs.nix-15693.url = "github:NixOS/nix/a94dee99e1805b1df24daefcdfa86a3d50c63685";
  # Nix master at the merge of NixOS/nix#15694, the first fix for #15693
  # ("insert or ignore" into SchemaMigrations).
  inputs.nix-15694.url = "github:NixOS/nix/c390460cdf7ee8b3208d982e09f91555f980759e";
  inputs.nix-after-16088.url = "github:NixOS/nix/fc7ac777d7afe93422032e2c9499c0612abb7354";
  # devenv main just before and at the merge of cachix/devenv#2296, which
  # fixed cachix/devenv#2281: a task's last lines of output going missing.
  inputs.devenv-before-2296 = {
    url = "github:cachix/devenv/cecb0452cacd9c524ccfc973d5caffff834cbf02";
    flake = false;
  };
  inputs.devenv-2296 = {
    url = "github:cachix/devenv/de0dc6a85ae88eb8194c2f7e053f3e933b77c2ac";
    flake = false;
  };

  outputs =
    {
      nixpkgs,
      nix-before-16088,
      nix-after-16088,
      nix-15693,
      nix-15694,
      devenv-before-2296,
      devenv-2296,
      ...
    }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
      lib = pkgs.lib;

      # LD_PRELOAD shim that stalls threads at random file, lock and socket
      # calls; see chaos-delay.c.
      chaosDelay = pkgs.runCommandCC "chaos-delay" { } ''
        mkdir -p $out/lib
        $CC -shared -fPIC -O2 -Wall -o $out/lib/libchaosdelay.so ${./chaos-delay.c} -ldl
      '';

      # Nix's functional test suite, configured but not built, running only
      # the named tests, each `iterations` times in a row. To have the tests
      # see 4 CPUs, run them with `rewind check --cores 4`.
      nixFunctionalTest =
        {
          components ? pkgs.nixVersions.nixComponents_2_35,
          tests,
          iterations ? 1,
          chaos ? false,
          chaosSeed ? 0,
          name ? builtins.concatStringsSep "-" tests,
          postPatch ? "",
          extraInputs ? [ ],
        }:
        components.nix-functional-tests.overrideAttrs (old: {
          pname = "nix-functional-${name}${lib.optionalString chaos "-chaos${toString chaosSeed}"}";
          preCheck = lib.optionalString chaos ''
            export LD_PRELOAD="''${LD_PRELOAD:+$LD_PRELOAD }${chaosDelay}/lib/libchaosdelay.so" CHAOS_SEED=${toString chaosSeed}
          '';
          postPatch = (old.postPatch or "") + postPatch;
          # Tools to have in the VM's closure for `rewind shell`, unused by the build.
          nativeBuildInputs = old.nativeBuildInputs ++ extraInputs;
          # The scripts we run by name need nothing that ninja would build;
          # the whole suite does (plugins, a libstore consumer).
          dontBuild = tests != [ ];
          checkPhase = ''
            runHook preCheck
            for i in $(seq 1 ${toString iterations}); do
              echo "iteration $i"
              meson test --no-rebuild --print-errorlogs ${builtins.concatStringsSep " " tests}
            done
            runHook postCheck
          '';
        });

      # One of Nix's unit test binaries, run as nixpkgs runs it, optionally
      # with a gtest filter. To size its thread pools for 4 CPUs, run it with
      # `rewind check --cores 4`.
      nixUnitTest =
        {
          components ? pkgs.nixVersions.nixComponents_2_35,
          suite,
          filter ? null,
          repeat ? 1,
        }:
        components.${suite}.tests.run.overrideAttrs (old: {
          name = "${suite}-run";
          buildCommand =
            lib.optionalString (filter != null) ''
              export GTEST_FILTER='${filter}'
            ''
            + ''
              export GTEST_REPEAT=${toString repeat}
            ''
            + old.buildCommand;
        });

      # The sink tests from Nix master, around the fix for NixOS/nix#16088.
      sinkTest =
        nixFlake:
        nixUnitTest {
          components = nixFlake.packages.x86_64-linux;
          suite = "nix-fetchers-tests";
          filter = "GitUtilsTest.sink*";
        };

      # Only the test that NixOS/nix#16088 deflaked, many times in one run.
      sinkNoParentDir =
        nixFlake:
        nixUnitTest {
          components = nixFlake.packages.x86_64-linux;
          suite = "nix-fetchers-tests";
          filter = "GitUtilsTest.sink_no_parent_dir";
          repeat = 100;
        };

      # git as nixpkgs builds it, with the build tree (test helpers, bin-wrappers
      # and t/) kept in $out/test-tree so the tests can run in another
      # derivation. The tree is copied back to the same /build path there.
      gitTestTree =
        (pkgs.git.override {
          withManual = false;
          doInstallCheck = false;
        }).overrideAttrs
          (old: {
            pname = "git-test-tree";
            postInstall = old.postInstall + ''
              mkdir -p $out/test-tree
              cp -a . $out/test-tree
              find $out/test-tree -name '*.o' -delete
              rm -rf $out/test-tree/target
              echo "$PWD" > $out/test-tree-path
            '';
            noAuditTmpdir = true;
            separateDebugInfo = false;
          });

      # Run some of git's test scripts from gitTestTree, each `iterations`
      # times, stopping at the first failure.
      gitTest =
        {
          name,
          scripts,
          iterations ? 1,
          tree ? gitTestTree,
        }:
        pkgs.runCommand "git-test-${name}"
          {
            nativeBuildInputs = [ pkgs.perl ];
          }
          ''
            src=$(cat ${tree}/test-tree-path)
            mkdir -p "$(dirname "$src")"
            cp -a ${tree}/test-tree "$src"
            chmod -R u+w "$src"
            cd "$src/t"
            for i in $(seq 1 ${toString iterations}); do
              echo "iteration $i"
              ${lib.concatMapStringsSep "\n" (s: "${pkgs.bash}/bin/bash ${s}") scripts}
            done
            touch $out
          '';

      # Nix functional tests that run Nix processes concurrently.
      concurrencyTests = [
        "binary-cache"
        "concurrent-builds"
        "gc-auto"
        "gc-concurrent"
        "gc-non-blocking"
        "gc-runtime"
        "nix-copy-ssh-ng"
        "repl"
        "tarball"
        "nix-profile"
      ];

      # Python with Sphinx's runtime and test dependencies, for running its
      # test suite from the source tree.
      sphinxTestPython = pkgs.python3.withPackages (
        ps:
        ps.sphinx.propagatedBuildInputs
        ++ [
          ps.defusedxml
          ps.pytest
          ps.pytest-xdist
          ps.typing-extensions
        ]
      );
      sphinxSrc = pkgs.python3Packages.sphinx.src;

      # curl as nixpkgs builds it, with the build tree (test servers, libtests
      # and tests/) kept in $out/test-tree, for running tests elsewhere.
      curlTestTree = pkgs.curl.overrideAttrs (old: {
        pname = "curl-test-tree";
        outputs = old.outputs ++ [ "testtree" ];
        # NTLM is off by default since curl 8.17; test 776 needs it.
        configureFlags = old.configureFlags ++ [ "--enable-ntlm" ];
        postBuild = (old.postBuild or "") + ''
          make -C tests/server -j$NIX_BUILD_CORES
          make -C tests/libtest -j$NIX_BUILD_CORES
          make -C tests/tunit -j$NIX_BUILD_CORES || true
          make -C tests/unit -j$NIX_BUILD_CORES || true
          make -C tests -j$NIX_BUILD_CORES || true
          # Let libtool relink its wrapped programs now, while the compiler
          # is at hand, instead of on their first run during the tests.
          for p in src/curl tests/libtest/libtests tests/server/servers; do
            ./$p --version > /dev/null 2>&1 || true
          done
        '';
        postInstall = old.postInstall + ''
          # An output of its own: the tree refers to bin, dev and out.
          mkdir -p $testtree/test-tree
          cp -a . $testtree/test-tree
          echo "$PWD" > $testtree/test-tree-path
        '';
        noAuditTmpdir = true;
        separateDebugInfo = false;
        dontStrip = true;
      });

      # Run some of curl's tests from curlTestTree with runtests.pl, each
      # `iterations` times, stopping at the first failure.
      curlTest =
        {
          name,
          tests,
          iterations ? 1,
          tree ? curlTestTree.testtree,
        }:
        pkgs.runCommand "curl-test-${name}"
          {
            nativeBuildInputs = [
              pkgs.perl
              pkgs.python3
              pkgs.openssl
              pkgs.diffutils
              pkgs.stunnel
            ];
          }
          ''
            src=$(cat ${tree}/test-tree-path)
            mkdir -p "$(dirname "$src")"
            cp -a ${tree}/test-tree "$src"
            chmod -R u+w "$src"
            cd "$src/tests"
            patchShebangs .
            for i in $(seq 1 ${toString iterations}); do
              echo "iteration $i"
              perl runtests.pl -n -p ${tests}
            done
            touch $out
          '';

      # A test for cachix/devenv#2281, added to devenv-tasks' tests: a task
      # that prints three lines and fails must report all three.
      devenvLastLinesTest = pkgs.writeText "devenv-last-lines-test.rs" ''

        #[tokio::test]
        async fn test_failed_task_keeps_last_lines() -> Result<(), Error> {
            let temp_dir = TempDir::new().unwrap();
            let db_path = temp_dir.path().join("tasks.db");
            let script = create_script("#!/bin/sh\necho line1\necho line2\necho line3\nexit 1\n")?;
            let tasks = Tasks::builder(
                Config::try_from(json!({
                    "roots": ["myapp:task_1"],
                    "run_mode": "all",
                    "tasks": [{ "name": "myapp:task_1", "command": script.to_str().unwrap() }]
                }))
                .unwrap(),
                VerbosityLevel::Verbose,
                Shutdown::new(),
            )
            .with_db_path(db_path)
            .build()
            .await?;
            tasks.run().await;
            match inspect_tasks(&tasks).await.as_slice() {
                [(_, TaskStatus::Completed(TaskCompleted::Failed(_, failure)))] => {
                    let lines: Vec<&str> = failure.stdout.iter().map(|(_, l)| l.as_str()).collect();
                    assert_eq!(lines, vec!["line1", "line2", "line3"]);
                }
                other => panic!("unexpected task statuses: {other:?}"),
            }
            Ok(())
        }
      '';

      # devenv-tasks' unit tests from a devenv commit, with
      # devenvLastLinesTest added, built on the host and installed as
      # $out/bin/devenv-tasks-tests. A release build as `cargo test --release`
      # makes it, with debug info, unstripped, and the crate's sources in
      # $out/src for gdb.
      devenvTasksTests =
        src:
        pkgs.rustPlatform.buildRustPackage {
          pname = "devenv-tasks-tests";
          version = "1.10.1";
          inherit src;
          cargoHash = "sha256-G8jhMHZW/zrYLNOXXIXkYFCVBlTLjW6pYJYmPE1qGGQ=";
          nativeBuildInputs = [ pkgs.jq ];
          postPatch = ''
            cat ${devenvLastLinesTest} >> devenv-tasks/src/tests/mod.rs
          '';
          buildPhase = ''
            runHook preBuild
            cargo test -p devenv-tasks --lib --release --no-run --message-format=json > cargo-test.json
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            install -Dm755 "$(jq -r 'select(.reason == "compiler-artifact" and .executable != null and .profile.test) | .executable' cargo-test.json)" \
              $out/bin/devenv-tasks-tests
            mkdir -p $out/src
            cp -r devenv-tasks $out/src/
            runHook postInstall
          '';
          doCheck = false;
          # devenv's release profile strips symbols; keep them and add DWARF.
          env.CARGO_PROFILE_RELEASE_DEBUG = "full";
          env.CARGO_PROFILE_RELEASE_STRIP = "none";
          dontStrip = true;
        };

      # Run devenv-tasks' tests matching `filter`, `iterations` times,
      # stopping at the first failure.
      devenvTasksTest =
        {
          name,
          src,
          filter ? "",
          iterations ? 1,
        }:
        let
          tests = devenvTasksTests src;
        in
        pkgs.runCommand "devenv-tasks-test-${name}" { } ''
          export HOME=$(mktemp -d)
          for i in $(seq 1 ${toString iterations}); do
            echo "iteration $i"
            ${tests}/bin/devenv-tasks-tests ${filter}
          done
          touch $out
        '';

      # The whole functional suite of Nix master as nixpkgs packages it, under
      # the chaos shim with each of these seeds.
      chaosSeeds = lib.range 1 16;

      unitSuites = [
        "nix-util-tests"
        "nix-store-tests"
        "nix-fetchers-tests"
        "nix-expr-tests"
        "nix-flake-tests"
      ];
    in
    {
      packages.x86_64-linux = {
        inherit
          chaosDelay
          gitTestTree
          sphinxTestPython
          sphinxSrc
          curlTestTree
          ;
        devenv-last-lines-before-2296 = devenvTasksTest {
          name = "last-lines-before-2296";
          src = devenv-before-2296;
          filter = "test_failed_task_keeps_last_lines";
        };
        devenv-last-lines-2296 = devenvTasksTest {
          name = "last-lines-2296";
          src = devenv-2296;
          filter = "test_failed_task_keeps_last_lines";
        };
        # All of devenv-tasks' tests at the fix.
        devenv-tasks-2296 = devenvTasksTest {
          name = "all-2296";
          src = devenv-2296;
        };
        curl-flaky = curlTest {
          name = "flaky";
          tests = "776 1510 587 573 1113 1162 1163 1631 1632";
        };
        git-t7900-strategy = gitTest {
          name = "t7900-strategy";
          scripts = [ "./t7900-maintenance.sh -v -x --run='maintenance.strategy is respected'" ];
        };
        sink-before-16088 = sinkTest nix-before-16088;
        sink-after-16088 = sinkTest nix-after-16088;
        sink-no-parent-dir-before-16088 = sinkNoParentDir nix-before-16088;
        sink-no-parent-dir-after-16088 = sinkNoParentDir nix-after-16088;
        nix-gc-non-blocking = nixFunctionalTest { tests = [ "gc-non-blocking" ]; };
        nix-concurrent-builds-15693 = nixFunctionalTest {
          components = nix-15693.packages.x86_64-linux;
          tests = [ "concurrent-builds" ];
        };
        nix-concurrent-builds-15693-chaos = nixFunctionalTest {
          components = nix-15693.packages.x86_64-linux;
          tests = [ "concurrent-builds" ];
          chaos = true;
        };
        nix-concurrent-builds = nixFunctionalTest { tests = [ "concurrent-builds" ]; };
        nix-concurrent-builds-chaos = nixFunctionalTest {
          tests = [ "concurrent-builds" ];
          chaos = true;
        };
        nix-concurrent-builds-15693-gdb = nixFunctionalTest {
          components = nix-15693.packages.x86_64-linux;
          tests = [ "concurrent-builds" ];
          name = "concurrent-builds-gdb";
          extraInputs = [ pkgs.gdb ];
        };
        nix-concurrent-builds-15693-gdb-chaos = nixFunctionalTest {
          components = nix-15693.packages.x86_64-linux;
          tests = [ "concurrent-builds" ];
          name = "concurrent-builds-gdb";
          extraInputs = [ pkgs.gdb ];
          chaos = true;
        };
        nix-concurrent-builds-15694 = nixFunctionalTest {
          components = nix-15694.packages.x86_64-linux;
          tests = [ "concurrent-builds" ];
        };
        nix-concurrent-builds-15694-chaos = nixFunctionalTest {
          components = nix-15694.packages.x86_64-linux;
          tests = [ "concurrent-builds" ];
          chaos = true;
        };
        nix-concurrent-builds-15693-x50 = nixFunctionalTest {
          components = nix-15693.packages.x86_64-linux;
          tests = [ "concurrent-builds" ];
          iterations = 50;
        };
        nix-concurrent-builds-15693-x10 = nixFunctionalTest {
          components = nix-15693.packages.x86_64-linux;
          tests = [ "concurrent-builds" ];
          iterations = 10;
        };
        nix-concurrency = nixFunctionalTest {
          name = "concurrency";
          tests = concurrencyTests;
        };
        nix-concurrency-chaos = nixFunctionalTest {
          name = "concurrency";
          tests = concurrencyTests;
          chaos = true;
        };
        nix-git-concurrency-chaos = nixFunctionalTest {
          components = pkgs.nixVersions.nixComponents_git;
          name = "concurrency";
          tests = concurrencyTests;
          chaos = true;
        };
        # Line 15 of Nix's gc-closure.sh in a loop, counting failures.
        pipefail-head = pkgs.runCommand "pipefail-head" { } ''
          ${pkgs.bash}/bin/bash ${./pipefail-head.sh} 2000 | tee $out
        '';
        nix-git-gc-closure = nixFunctionalTest {
          components = pkgs.nixVersions.nixComponents_git;
          tests = [ "gc-closure" ];
        };
        nix-gc-closure = nixFunctionalTest { tests = [ "gc-closure" ]; };
        # gc-closure.sh without the pipe into head -n1.
        nix-git-gc-closure-fixed = nixFunctionalTest {
          components = pkgs.nixVersions.nixComponents_git;
          tests = [ "gc-closure" ];
          name = "gc-closure-fixed";
          postPatch = ''
            substituteInPlace gc-closure.sh \
              --replace-fail 'input2_out=$(printf "%s" "$input2" | head -n1)' \
                             'input2_out=$(head -n1 <<< "$input2")'
          '';
        };
        nix-git-gc-closure-x200 = nixFunctionalTest {
          components = pkgs.nixVersions.nixComponents_git;
          tests = [ "gc-closure" ];
          iterations = 200;
        };
        nix-gc-closure-x200 = nixFunctionalTest {
          tests = [ "gc-closure" ];
          iterations = 200;
        };
        nix-git-all = nixFunctionalTest {
          components = pkgs.nixVersions.nixComponents_git;
          name = "all";
          tests = [ ];
        };
        nix-gc-non-blocking-x5 = nixFunctionalTest {
          tests = [ "gc-non-blocking" ];
          iterations = 5;
        };
      }
      // lib.listToAttrs (
        map (
          seed:
          lib.nameValuePair "nix-git-all-chaos${toString seed}" (nixFunctionalTest {
            components = pkgs.nixVersions.nixComponents_git;
            name = "all";
            tests = [ ];
            chaos = true;
            chaosSeed = seed;
          })
        ) chaosSeeds
      )
      // lib.listToAttrs (
        map (
          suite:
          lib.nameValuePair suite (nixUnitTest {
            inherit suite;
          })
        ) unitSuites
      );
    };
}
