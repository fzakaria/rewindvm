# The builder of a derivation that `inRewind` rewrote (nix/lib.nix). Nix
# starts this script in its sandbox in place of the derivation's own
# builder. The script describes the build Nix has just set up, in the form
# Nix's external-builders setting hands a program, and gives the
# description to `rewind build`, which runs the derivation's own builder in
# the VM and copies the outputs back.
#
# Usage: in-rewind.sh <rewind> <jq> <closure> <inputs> <builder> [<arg>...]
#
# <closure> lists the store paths the VM may hold, one a line: the closure
# of <inputs>, a file that names every store path the derivation names.
# <builder> and its arguments are the derivation's own.
set -euo pipefail

rewind=$1 jq=$2 closure=$3 inputs=$4
shift 4

# The environment below names the build directory, and the VM has one at
# /build, where the sandbox puts this one. Without the sandbox the
# directory is wherever this machine keeps builds.
if [ "$NIX_BUILD_TOP" != /build ]; then
  echo "inRewind: the build directory is $NIX_BUILD_TOP, not /build; build with Nix's sandbox on" >&2
  exit 1
fi

# The environment as Nix gave it to this builder. bash has added variables
# of its own to what its children see since, so the description takes the
# environment bash itself was started with.
exec 3< /proc/self/environ

# With structured attributes Nix writes the outputs' paths to a file, and
# otherwise to a variable for each output that `outputs` names.
attrs=${NIX_ATTRS_JSON_FILE:-/dev/null}

# The description, version 1. The store paths are the ones the derivation
# names and never whatever the sandbox shows under /nix/store: each
# machine's nix.conf adds paths of its own there, and a run whose root held
# them would differ from machine to machine. The file that listed the
# paths is no input of the derivation, so the file stays out as well.
# /tmp, the sandbox's own, is the scratch directory the description and
# the run are kept in. The builder and its arguments follow `--`, or jq
# would take one that starts with a dash, as bash's -e does, for an option
# of its own.
"$jq" --null-input \
  --rawfile environ /dev/fd/3 \
  --rawfile closure "$closure" \
  --slurpfile attrs "$attrs" \
  --arg inputs "$inputs" \
  '
  ( $environ | split("\u0000")
    | map(select(length > 0) | split("=") | {key: .[0], value: (.[1:] | join("="))})
    | from_entries ) as $env
  | {
      version: 1,
      builder: $ARGS.positional[0],
      args: $ARGS.positional[1:],
      env: $env,
      inputPaths: ($closure | split("\n") | map(select(length > 0 and . != $inputs))),
      outputs: (
        if $attrs[0] then $attrs[0].outputs
        else $env.outputs | split(" ") | map(select(length > 0) | {key: ., value: $env[.]}) | from_entries
        end
      ),
      storeDir: $env.NIX_STORE,
      realStoreDir: $env.NIX_STORE,
      system: ($env.system // $attrs[0].system),
      tmpDir: $env.NIX_BUILD_TOP,
      tmpDirInSandbox: $env.NIX_BUILD_TOP,
      topTmpDir: "/tmp"
    }
  ' --args -- "$@" > /tmp/build.json
exec 3<&-

exec "$rewind" build /tmp/build.json
