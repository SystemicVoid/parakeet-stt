#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

fail() {
    echo "stt-helper flag check failed: $*" >&2
    exit 1
}

assert_option_value() {
    local flag="$1"
    local expected="$2"
    local -n tokens_ref="$3"
    local idx
    for idx in "${!tokens_ref[@]}"; do
        if [ "${tokens_ref[$idx]}" = "$flag" ]; then
            if [ "$((idx + 1))" -ge "${#tokens_ref[@]}" ]; then
                fail "missing value after $flag"
            fi
            if [ "${tokens_ref[$((idx + 1))]}" != "$expected" ]; then
                fail "expected $flag=$expected, got ${tokens_ref[$((idx + 1))]}"
            fi
            return 0
        fi
    done
    fail "missing option token $flag"
}

assert_option_missing() {
    local flag="$1"
    local -n tokens_ref="$2"
    local token
    for token in "${tokens_ref[@]}"; do
        if [ "$token" = "$flag" ]; then
            fail "unexpected option token $flag"
        fi
    done
}

bash -n "$repo_root/scripts/stt-helper.sh"

# shellcheck source=./stt-helper.sh
source "$repo_root/scripts/stt-helper.sh"

mapfile -t stable_options < <(stt __start-option-names-stable)
[ "${#stable_options[@]}" -gt 0 ] || fail "no stable metadata options were returned"
mapfile -t deprecated_options < <(stt __start-option-names-deprecated)

start_help="$(stt start --help)"
for opt in "${stable_options[@]}"; do
    if ! grep -Fq -- "--$opt " <<<"$start_help"; then
        fail "help output is missing --$opt"
    fi
done
for opt in "${deprecated_options[@]}"; do
    if grep -Fq -- "--$opt " <<<"$start_help"; then
        fail "stable help unexpectedly includes deprecated --$opt"
    fi
done

compat_help="$(stt start --help-compat)"
for opt in "${deprecated_options[@]}"; do
    if ! grep -Fq -- "--$opt " <<<"$compat_help"; then
        fail "compat help output is missing --$opt"
    fi
done

mapfile -t default_args < <(stt __start-args)
if ! printf "%s\n" "${default_args[@]}" | grep -Fxq -- "--endpoint"; then
    fail "__start-args output is missing --endpoint"
fi

for opt in "${stable_options[@]}"; do
    sample="ci_${opt//[^a-zA-Z0-9]/_}"
    mapfile -t override_args < <(stt __start-args "--$opt" "$sample")
    assert_option_value "--$opt" "$sample" override_args
done

for opt in "${deprecated_options[@]}"; do
    sample="ci_${opt//[^a-zA-Z0-9]/_}"
    mapfile -t override_args < <(stt __start-args "--$opt" "$sample" 2>/dev/null)
    assert_option_missing "--$opt" override_args
done

mapfile -t alias_args < <(stt __start-args --type)
assert_option_value "--injection-mode" "type" alias_args

set +e
unknown_output="$(stt __start-args --definitely-not-a-real-flag 2>&1)"
unknown_status=$?
set -e
[ "$unknown_status" -ne 0 ] || fail "unknown flag unexpectedly succeeded"
grep -Fq "stt start --help" <<<"$unknown_output" || fail "unknown flag error does not point to start help"

main_help="$(stt --help)"
grep -Fq "stt start --help" <<<"$main_help" || fail "main help is missing start-help shortcut"

echo "stt-helper metadata checks passed."
