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
# - the line is split into simple commands at top-level ; | & ( ) { } and
#   backticks (redirections such as 2>&1 are not separators);
# - in each command, leading `VAR=value` assignments, option words, and
#   the wrappers exec/nohup/setsid/env/sudo/command/time/timeout <args>,
#   and the keywords if/then/do/else/elif/while/until/!/{ are skipped to
#   find the command word;
# - a command launches kwe-daemon when its command word is a kwe-daemon
#   path or bare `kwe-daemon`, or — review fixes D, R-b — when ANY of its
#   words names a kwe-daemon binary (an unquoted path ending in
#   /kwe-daemon, or a variable whose assignment ANYWHERE in the same
#   script points at such a path, directly, through `$(command -v
#   kwe-daemon)`/`$(which ...)`, or through another such variable), so
#   wrappers (nice, stdbuf, ionice, chrt, xargs, a shell function, ...)
#   are covered without listing them; `bash|sh|zsh|dash -c` and `eval`
#   strings are searched too; and any `cargo run` whose command mentions
#   kwe-daemon (-p/--bin/--manifest-path ...) is a launch. Commands that
#   only look at a binary (test/[[/echo/printf/cp/install/pkill/pgrep/
#   kill/systemctl/... — the INERT list) and pure assignments are not.
# A launch passes when that same command (not its comment) has
# `--no-led-worker` as a word of its own, or `--led-sdk-port` whose value
# provably is not 6742: a literal number (leading zeros count: 06742 is
# 6742), or a variable whose every assignment in the script is a literal
# number other than 6742, or a variable the script explicitly refuses to
# be 6742 (a line comparing it with ==/!=/-eq/-ne against 6742 or a
# variable assigned 6742). Anything else — arithmetic, command
# substitution, `${v:-...}`, an unassigned variable — is suspect. The
# explicit `# led-guard: <why>` marker on the line opts in deliberately.
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
            || ((c == "{" || c == "}") && prev ~ /[[:space:];]/ && nxt ~ /[[:space:];]|^$/) \
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

# Pass 1 (NR == FNR): collect assignments name -> value (every leading
# NAME=value word of a command, so `A=1 B=2 cmd` records both), and keep
# each logical line for the refusal-check search.
function collect(text,    seg, n, j, s, w, nw, k, eq, name) {
    lines[++nlines] = text
    n = split_commands(text, seg)
    for (j = 1; j <= n; j++) {
        s = trim(seg[j])
        while (s ~ /^(export|local|readonly|declare|typeset)([[:space:]]+-[A-Za-z]+)*[[:space:]]+/)
            sub(/^[a-z]+([[:space:]]+-[A-Za-z]+)*[[:space:]]+/, "", s)
        nw = split_words(s, w)
        for (k = 1; k <= nw; k++) {
            if (w[k] !~ /^[A-Za-z_][A-Za-z_0-9]*=/) break
            eq = index(w[k], "=")
            name = substr(w[k], 1, eq - 1)
            assigned[name, ++count[name]] = trim(unquote(substr(w[k], eq + 1)))
        }
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

# Commands that only inspect, copy, signal or print the path of a binary.
function inert(u) {
    return u ~ /^(\[\[|\[|test|echo|printf|cp|mv|rm|ln|install|ls|stat|file|strip|ldd|chmod|chown|touch|readlink|realpath|basename|dirname|grep|rg|sha256sum|md5sum|du|wc|cat|head|tail|pkill|pgrep|kill|killall|wait|systemctl|journalctl|fail|die|warn|note|info|for|case|unset|jq)$/
}

# A word that names a kwe-daemon binary: a path ending in /kwe-daemon, or
# exactly one expansion of a variable assigned such a path.
function names_daemon(u,    name) {
    if (u ~ /\/kwe-daemon$/) return 1
    name = var_name(u)
    return name != "" && (name in dvar) && u ~ /^\$\{?[A-Za-z_][A-Za-z_0-9]*\}?$/
}

# A `-c` / eval string that runs kwe-daemon (any path, bare name, or a
# daemon variable).
function string_launches(u,    name, pat) {
    # Built from two strings so this line is not itself a "launch".
    pat = "(^|[^A-Za-z_0-9.-])kwe-" "daemon([^A-Za-z_0-9.-]|$)"
    if (u ~ pat) return 1
    for (name in dvar)
        if (u ~ ("\\$\\{?" name "([^A-Za-z_0-9]|$)")) return 1
    return 0
}

function launches(s,    w, n, k, word, cu, u, shell, cargo) {
    word = command_word(s)
    if (word == "") return 0
    cu = unquote(word)
    # The command word itself (bare `kwe-daemon` counts only here).
    if (cu !~ /[[:space:]=]/ && (cu ~ /(^|\/)kwe-daemon$/ || names_daemon(cu))) return 1
    if (inert(cu)) return 0
    shell = cu ~ /^(bash|sh|zsh|dash|eval)$/
    n = split_words(s, w)
    cargo = 0
    for (k = 1; k <= n; k++) {
        u = unquote(w[k])
        if (u ~ /[[:space:]]/) {
            # A string with spaces is a message, unless a shell runs it.
            if (shell && string_launches(u)) return 1
            continue
        }
        # NAME=value / --option=value words are not the program run.
        if (u ~ /=/) continue
        if (names_daemon(u)) return 1
        if (u == "cargo") cargo = 1
        else if (cargo && u == "run" && s ~ /kwe-daemon/) return 1
    }
    return 0
}

# Value of --led-sdk-port in command `s` ("" when absent, SUSPECT when the
# flag has no value).
function sdk_port_value(s,    w, n, k) {
    n = split_words(s, w)
    for (k = 1; k <= n; k++) {
        if (w[k] == "--led-sdk-port") return k < n ? w[k + 1] : "SUSPECT"
        if (w[k] ~ /^--led-sdk-port=/) return substr(w[k], length("--led-sdk-port=") + 1)
    }
    return ""
}

# Every assignment of `name` is the literal 6742.
function real_port_var(name,    i) {
    if (!(name in count)) return 0
    for (i = 1; i <= count[name]; i++)
        if (assigned[name, i] !~ /^0*6742$/) return 0
    return 1
}

# Some line of the script compares `name` with 6742 (a refusal check such
# as `(( port != 6742 )) || exit` or `[[ $port == 6742 ]] && exit`).
function refused(name,    i, t, other, words, nw, k) {
    for (i = 1; i <= nlines; i++) {
        t = lines[i]
        if (t !~ ("(^|[^A-Za-z_0-9])" name "([^A-Za-z_0-9]|$)")) continue
        if (t !~ /(==|!=|-eq|-ne)/) continue
        if (t ~ /(^|[^0-9])0*6742([^0-9]|$)/) return 1
        nw = split(t, words, /[^A-Za-z_0-9]+/)
        for (k = 1; k <= nw; k++)
            if (words[k] != name && words[k] != "" && real_port_var(words[k])) return 1
    }
    return 0
}

# "ok", "real" (resolves to 6742) or "suspect".
function port_verdict(v,    u, name, i, literal) {
    u = unquote(v)
    if (u ~ /^[0-9]+$/) return (u + 0 == 6742) ? "real" : "ok"
    if (u !~ /^\$\{?[A-Za-z_][A-Za-z_0-9]*\}?$/) return "suspect"
    name = var_name(u)
    if (real_port_var(name)) return "real"
    literal = (name in count)
    if (literal)
        for (i = 1; i <= count[name]; i++) {
            if (assigned[name, i] ~ /^0*6742$/) return "real"
            if (assigned[name, i] !~ /^[0-9]+$/) literal = 0
        }
    if (literal || refused(name)) return "ok"
    return "suspect"
}

function has_word(s, want,    w, n, k) {
    n = split_words(s, w)
    for (k = 1; k <= n; k++) if (unquote(w[k]) == want) return 1
    return 0
}

function check(text, line,    seg, n, j, s, marked, v, verdict) {
    n = split_commands(text, seg)
    marked = COMMENT ~ /^[[:space:]]*led-guard: [^[:space:]]/
    for (j = 1; j <= n; j++) {
        s = seg[j]
        if (!launches(s)) continue
        if (marked) continue
        v = sdk_port_value(s)
        if (v != "") {
            verdict = port_verdict(v)
            if (verdict == "ok") continue
            if (verdict == "real") {
                printf "%s:%d: kwe-daemon launched with --led-sdk-port 6742 (the real OpenRGB port)\n", FILENAME, line
            } else {
                printf "%s:%d: kwe-daemon launched with an --led-sdk-port value not provably other than 6742 (%s)\n", FILENAME, line, v
            }
            continue
        }
        if (has_word(s, "--no-led-worker")) continue
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
        # Review fixes D (R-b): wrappers, shells, ports the guard cannot prove.
        $'run(){ "$@"; }; run target/debug/kwe-daemon --socket s'
        $'run() { "$@"; }\nrun target/debug/kwe-daemon --socket s'
        $'nice -n 5 target/debug/kwe-daemon --socket s'
        $'stdbuf -oL target/debug/kwe-daemon --socket s'
        $'ionice -c3 "$t/debug/kwe-daemon" --socket s'
        $'chrt -i 0 target/debug/kwe-daemon --socket s'
        $'xargs target/debug/kwe-daemon --socket s < /dev/null'
        $'DAEMON=target/debug/kwe-daemon\nnice "$DAEMON" --socket s'
        $'bash -c "target/debug/kwe-daemon --socket s"'
        $'sh -c \'kwe-daemon --socket s\''
        $'DAEMON=target/debug/kwe-daemon\neval "$DAEMON --socket s"'
        $'PORT=6742 target/debug/kwe-daemon --led-sdk-port "$PORT" --socket s'
        $'PORT=6742\ntarget/debug/kwe-daemon --led-sdk-port "$PORT" --socket s'
        $'target/debug/kwe-daemon --socket s --led-sdk-port=06742'
        $'target/debug/kwe-daemon --led-sdk-port $((6700+42)) --socket s'
        $'target/debug/kwe-daemon --led-sdk-port "$(echo 6742)" --socket s'
        $'target/debug/kwe-daemon --led-sdk-port "${port:-6742}" --socket s'
        $'target/debug/kwe-daemon --led-sdk-port "$stub_port" --socket s'
        $'p=6743\np=$(cat f)\ntarget/debug/kwe-daemon --led-sdk-port "$p"'
        $'target/debug/kwe-daemon --socket s --led-sdk-port'
        $'cargo run --manifest-path crates/kwe-daemon/Cargo.toml -- --socket s'
        $'cargo run --release -p kwe-daemon --bin kwe-daemon -- --socket s'
        $'nice cargo run -p kwe-daemon -- --socket s'
        $'target/debug/kwe-daemon --socket "a --no-led-worker b"'
        $'target/debug/kwe-daemon --socket s --no-led-worker=false'
        $'echo "--no-led-worker"; target/debug/kwe-daemon --socket s'
        $'if true; then target/debug/kwe-daemon --socket s; fi'
        $'"target/debug/kwe-daemon" --socket s'
        $'d=target/debug; "$d/kwe-daemon" --socket s'
    )
    local -a good=(
        $'target/debug/kwe-daemon --no-led-worker --socket s'
        $'daemon_bin="$root/target/debug/kwe-daemon"\n"$daemon_bin" \\\n  --socket s \\\n  --no-led-worker &'
        $'stub_port=6743\ntarget/debug/kwe-daemon --led-sdk-port "$stub_port" --socket s'
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
        # Review fixes D (R-b): ports proven not 6742, wrappers with the flag,
        # commands that only look at the binary.
        $'readonly REAL=6742\nport="$(pick)"\n(( port != REAL )) || exit 1\n"$t/debug/kwe-daemon" \\\n    --led-sdk-port "$port" &'
        $'if [[ "${DEV_PORT:-}" == "6742" ]]; then exit 2; fi\ntarget/debug/kwe-daemon --led-sdk-port "$DEV_PORT" --socket s'
        $'target/debug/kwe-daemon --led-sdk-port=06743 --socket s'
        $'nice -n 5 target/debug/kwe-daemon --no-led-worker --socket s'
        $'run(){ "$@"; }; run target/debug/kwe-daemon --no-led-worker'
        $'bash -c "echo kwe-daemon-like; true"'
        $'cargo run --manifest-path crates/kwe-daemon/Cargo.toml -- --no-led-worker'
        $'pgrep -f -- "target/debug/kwe-daemon --socket" || true'
        $'[[ -x "$t/debug/kwe-daemon" ]] || fail "target/debug/kwe-daemon missing"'
        $'install -Dm755 target/release/kwe-daemon "$pkg/usr/bin/kwe-daemon"'
        $'for binary in kwe-daemon kwe kwe-led-worker; do [[ -x "target/debug/$binary" ]]; done'
        $'systemctl --user is-active kwe-daemon'
        $'fail "kwe-daemon did not start: target/debug/kwe-daemon"'
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
