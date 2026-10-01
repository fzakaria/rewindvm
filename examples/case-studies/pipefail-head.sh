# Line 15 of Nix's tests/functional/gc-closure.sh, in a loop: count how often
# the command substitution fails under `set -o pipefail`.
set -u -o pipefail
input2='/build/nix-test/main/gc-closure/store/ij8qwx0m6dvpmggqmq8zmhbaiw6ym8pg-dependencies-input-2
/build/nix-test/main/gc-closure/store/hw6dm6rpynd7hmhl1mnp0z4m28lfbx4b-dependencies-input-2-out2'
runs=$1
fails=0
for ((i = 0; i < runs; i++)); do
    if ! input2_out=$(printf "%s" "$input2" | head -n1); then
        fails=$((fails + 1))
    fi
done
echo "$fails of $runs failed"
