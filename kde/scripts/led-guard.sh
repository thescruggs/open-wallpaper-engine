#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# LED guard (OpenRGB review fixes B MUST-FIX 4, hardened in C2 / R3).
#
# LED sync defaults on, so a script that starts kwe-daemon without
# --no-led-worker would drive the maintainer's REAL devices through OpenRGB's
# SDK server (127.0.0.1:6742). Every kwe-daemon launch in the given scripts
# must pass --no-led-worker, or point the worker at a stub with
# --led-sdk-port <port> where <port> is not 6742. A deliberate real-device
# launch (dev-run.sh's KWE_DEV_LED=1) must say so with a trailing
# `# led-guard: <why>` comment.
#
# Usage: scripts/led-guard.sh FILE...    prints file:line for each violation,
#                                         exits 1 if there is any
#        scripts/led-guard.sh --self-test checks the guard against crafted
#                                         good/bad snippets (run by check.sh)
#
# How a launch is recognised, per logical line (backslash continuations
# joined; the trailing comment stripped first, quotes respected):
# - the line is split into simple commands at top-level ; | & ( ) and
#   backticks (redirections such as 2>&1 are not separators);
# - in each command, leading `VAR=value` assignments, option words, and
#   the wrappers exec/nohup/setsid/env/sudo/command/time/timeout <args>,
#   and the
#   keywords if/then/do/else/elif/while/until/!/{ are skipped;
# - the first remaining word launches kwe-daemon when, unquoted, it is a
#   path ending in kwe-daemon (or bare kwe-daemon), when it expands a
#   variable whose assignment ANYWHERE in the same script points at such a
#   path (directly, through `$(command -v kwe-daemon)`/`$(which ...)`, or
#   through another such variable), or when it is `cargo run` for
#   -p/--package/--bin kwe-daemon.
# The flags must be in that same command (not in its comment).
set -euo pipefail

guard_awk='
function trim(t) { sub(/^[[:space:]]+/, "", t); sub(/[[:space:]]+$/, "", t); return t }
function unquote(t) { gsub(/["\047]/, "", t); return t }

# Splits `text` into top-level simple commands seg[1..n] and returns n; sets
# COMMENT to the trailing comment (without the #), or "".
function split_commands(text, seg,    n, i, c, q, cur, prev, nxt, depth, len) {
    n = 0; q = ""; cur = ""; COMMENT = ""; prev = " "; depth = 0
    len = length(text)
    for (i = 1; i <= len; i++) {
        c = substr(text, i, 1)
        if (q != "") {
            cur = cur c
            if (c == "\\" && q == "\"") { i++; cur = cur substr(text, i, 1) }
            else if (c == q) q = ""
            prev = c
            continue
        }
        if (c == "\\") { cur = cur c substr(text, i + 1, 1); i++; prev = "x"; continue }
        if (c == "\"" || c == "\047") { q = c; cur = cur c; prev = c; continue }
        if (c == "#" && prev ~ /[[:space:];|&()]/) { COMMENT = substr(text, i + 1); break }
        nxt = substr(text, i + 1, 1)
        # $( ... ) stays inside the word it belongs to (assignments).
        if (c == "$" && nxt == "(") { depth++; cur = cur "$("; i++; prev = "("; continue }
        if (c == ")" && depth > 0) { depth--; cur = cur c; prev = c; continue }
        if (c == ";" || c == "|" || c == "(" || c == ")" || c == "`" \
            || (c == "&" && prev != ">" && prev != "<" && nxt != ">")) {
            seg[++n] = cur; cur = ""; prev = " "
            continue
        }
        cur = cur c; prev = c
    }
    seg[++n] = cur
    return n
}

# Quote-aware word split of one simple command into w[1..n].
function split_words(s, w,    n, i, c, q, cur, len) {
    n = 0; q = ""; cur = ""; len = length(s)
    for (i = 1; i <= len; i++) {
        c = substr(s, i, 1)
        if (q != "") {
            cur = cur c
            if (c == "\\" && q == "\"") { i++; cur = cur substr(s, i, 1) }
            else if (c == q) q = ""
            continue
        }
        if (c == "\"" || c == "\047") { q = c; cur = cur c; continue }
        if (c == "\\") { cur = cur c substr(s, i + 1, 1); i++; continue }
        if (c ~ /[[:space:]]/) { if (cur != "") { w[++n] = cur; cur = "" }; continue }
        cur = cur c
    }
    if (cur != "") w[++n] = cur
    return n
}

# The command word of a simple command, after assignments and wrappers.
function command_word(s,    w, n, k, word) {
    n = split_words(s, w)
    for (k = 1; k <= n; k++) {
        word = w[k]
        if (word ~ /^[A-Za-z_][A-Za-z_0-9]*=/) continue
        # Wrapper options (env -i, env -u NAME, exec -a NAME, ...).
        if (word ~ /^-/) { if (word ~ /^-[uCaS]$/) k++; continue }
        if (word ~ /^(exec|nohup|setsid|env|sudo|command|builtin|time|if|then|do|else|elif|while|until|!|\{)$/) continue
        if (word == "timeout") {
            # Options (and the values of -k/-s), then the duration.
            k++
            while (k <= n && w[k] ~ /^-/) {
                if (w[k] ~ /^(-k|-s|--kill-after|--signal)$/) k++
                k++
            }
            continue
        }
        return word
    }
    return ""
}

function daemon_path(v) {
    return v ~ /(^|\/)kwe-daemon$/ \
        || v ~ /^\$\((command -v|which|type -P|readlink -f)[[:space:]]+[^)]*kwe-daemon\)$/
}

function var_name(u,    name) {
    if (u !~ /^\$\{?[A-Za-z_]/) return ""
    name = u
    sub(/^\$\{?/, "", name)
    match(name, /^[A-Za-z_][A-Za-z_0-9]*/)
    return substr(name, 1, RLENGTH)
}

# Pass 1 (NR == FNR): collect assignments name -> value.
function collect(text,    seg, n, j, s, eq, name, value) {
    n = split_commands(text, seg)
    for (j = 1; j <= n; j++) {
        s = trim(seg[j])
        while (s ~ /^(export|local|readonly|declare|typeset)([[:space:]]+-[A-Za-z]+)*[[:space:]]+/)
            sub(/^[a-z]+([[:space:]]+-[A-Za-z]+)*[[:space:]]+/, "", s)
        if (s !~ /^[A-Za-z_][A-Za-z_0-9]*=/) continue
        eq = index(s, "=")
        name = substr(s, 1, eq - 1)
        value = trim(unquote(substr(s, eq + 1)))
        assigned[name, ++count[name]] = value
    }
}

function resolve(    changed, round, key, parts, name, value, ref) {
    for (round = 0; round < 16; round++) {
        changed = 0
        for (key in assigned) {
            split(key, parts, SUBSEP)
            name = parts[1]
            if (name in dvar) continue
            value = assigned[key]
            ref = var_name(value)
            if (daemon_path(value) || (ref != "" && (ref in dvar) \
                && value ~ /^\$\{?[A-Za-z_][A-Za-z_0-9]*\}?$/)) {
                dvar[name] = 1; changed = 1
            }
        }
        if (!changed) break
    }
}

function launches(s,    word, u, name) {
    word = command_word(s)
    if (word == "") return 0
    u = unquote(word)
    # One quoted word holding a whole command line (e.g. a string literal)
    # is not a command path.
    if (u ~ /[[:space:]=]/) return 0
    if (u ~ /(^|\/)kwe-daemon$/) return 1
    name = var_name(u)
    if (name != "" && (name in dvar)) return 1
    if (u == "cargo" && s ~ /(^|[[:space:]])run([[:space:]]|$)/ \
        && s ~ /(-p|--package|--bin)([[:space:]]+|=)?["\047]?kwe-daemon(["\047[:space:]]|$)/) return 1
    return 0
}

function check(text, line,    seg, n, j, s, marked) {
    n = split_commands(text, seg)
    marked = COMMENT ~ /^[[:space:]]*led-guard: [^[:space:]]/
    for (j = 1; j <= n; j++) {
        s = seg[j]
        if (!launches(s)) continue
        if (marked) continue
        if (s ~ /--led-sdk-port([[:space:]]+|=)["\047]?6742(["\047[:space:]]|$)/) {
            printf "%s:%d: kwe-daemon launched with --led-sdk-port 6742 (the real OpenRGB port)\n", FILENAME, line
            continue
        }
        if (s ~ /(^|[[:space:]])["\047]?--no-led-worker(["\047[:space:]]|$)/) continue
        if (s ~ /--led-sdk-port([[:space:]]+|=)[^[:space:]]/) continue
        printf "%s:%d: kwe-daemon launched without --no-led-worker or a stub --led-sdk-port\n", FILENAME, line
    }
}

FNR == 1 && NR != FNR && !resolved { resolve(); resolved = 1 }
{
    line = $0
    if (line ~ /\\[[:space:]]*$/) {
        sub(/\\[[:space:]]*$/, "", line)
        joined = joined line " "
        if (!start) start = FNR
        next
    }
    joined = joined line
    if (!start) start = FNR
    if (NR == FNR) collect(joined); else check(joined, start)
    joined = ""; start = 0
}
'

run_guard() {
    local file failures="" out
    for file in "$@"; do
        # Each file is read twice: pass 1 collects assignments.
        out="$(awk "$guard_awk" "$file" "$file")"
        [[ -n "$out" ]] && failures+="$out"$'\n'
    done
    if [[ -n "$failures" ]]; then
        printf '%s' "$failures"
        return 1
    fi
    return 0
}

self_test() {
    local dir status=0 i=0 name snippet
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' RETURN
    local -a bad=(
        $'DAEMON=target/release/kwe-daemon\n"$DAEMON" --socket s'
        $'cargo run -p kwe-daemon -- --socket s'
        $'cargo run --bin kwe-daemon -- --socket s'
        $'d=target/debug/kwe-daemon; $d --socket s'
        $'target/debug/kwe-daemon --socket s # --no-led-worker'
        $'target/debug/kwe-daemon --socket s \\\n    --state-dir x # --no-led-worker'
        $'target/debug/kwe-daemon --led-sdk-port 6742'
        $'target/debug/kwe-daemon --led-sdk-port=6742 --socket s'
        $'"$target_dir/debug/kwe-daemon" --led-sdk-port "6742"'
        $'daemon_bin="$root/target/debug/kwe-daemon"\n"$daemon_bin" \\\n  --socket s \\\n  --state-dir x &'
        $'kwe-daemon --socket s'
        $'env FOO=1 nohup target/debug/kwe-daemon --socket s &'
        $'mkdir -p x && target/debug/kwe-daemon --socket s'
        $'bin="$(command -v kwe-daemon)"; "$bin" --socket s'
        $'real=target/debug/kwe-daemon\nalias_bin="$real"\n"${alias_bin}" --socket s'
        $'STEAM_ROOT="$r" "$t/debug/kwe-daemon" --socket s 2>&1 &'
        $'timeout -k 1 5 target/debug/kwe-daemon --socket s'
        $'env -i \\\n    HOME="$HOME" \\\n    "$t/debug/kwe-daemon" \\\n    --socket s &'
        $'exec -a kwe target/debug/kwe-daemon --socket s'
        $'echo started; target/debug/kwe-daemon --socket s --no-led-worker-x'
        $'target/debug/kwe-daemon --no-led-worker; target/debug/kwe-daemon --socket s'
    )
    local -a good=(
        $'target/debug/kwe-daemon --no-led-worker --socket s'
        $'daemon_bin="$root/target/debug/kwe-daemon"\n"$daemon_bin" \\\n  --socket s \\\n  --no-led-worker &'
        $'target/debug/kwe-daemon --led-sdk-port "$stub_port" --socket s'
        $'target/debug/kwe-daemon --led-sdk-port 6743'
        $'systemctl --user restart kwe-daemon'
        $'systemctl --user restart kwe-daemon.service'
        $'[[ -x target/debug/kwe-daemon ]] || exit 1'
        $'# target/debug/kwe-daemon --socket s'
        $'echo "run target/debug/kwe-daemon --socket s"'
        $'target/debug/kwe-daemon --socket s  # led-guard: deliberate real devices'
        $'daemon_bin=target/debug/kwe-daemon'
        $'cargo build -p kwe-daemon'
        $'cargo run -p kwe-daemon -- --no-led-worker'
        $'pkill -f target/debug/kwe-daemon'
        $'for b in kwe-daemon kwe; do echo "$b"; done'
        $'"$target_dir/debug/kwe-daemon" --socket s --no-led-worker 2>&1 &'
        $'DAEMON=target/debug/kwe-daemon\necho "$DAEMON"'
        $'cp target/debug/kwe-daemon "$pkg/usr/bin/kwe-daemon"'
        $'env -i \\\n    HOME="$HOME" \\\n    "$t/debug/kwe-daemon" \\\n    --no-led-worker &'
    )
    for snippet in "${bad[@]}"; do
        i=$((i + 1)); name="$dir/bad-$i.sh"
        printf '%s\n' "$snippet" >"$name"
        if run_guard "$name" >/dev/null; then
            echo "led-guard self-test: MISSED bad snippet $i:" >&2
            printf '    %s\n' "$snippet" >&2
            status=1
        fi
    done
    i=0
    for snippet in "${good[@]}"; do
        i=$((i + 1)); name="$dir/good-$i.sh"
        printf '%s\n' "$snippet" >"$name"
        if ! run_guard "$name" >/dev/null; then
            echo "led-guard self-test: FALSE POSITIVE on good snippet $i:" >&2
            printf '    %s\n' "$snippet" >&2
            status=1
        fi
    done
    if [[ "$status" == 0 ]]; then
        echo "led-guard self-test: ${#bad[@]} bad snippets caught, ${#good[@]} good snippets passed"
    fi
    return "$status"
}

if [[ "${1:-}" == "--self-test" ]]; then
    self_test
    exit $?
fi
if [[ "$#" -eq 0 ]]; then
    echo "usage: $0 FILE... | --self-test" >&2
    exit 2
fi
run_guard "$@"
