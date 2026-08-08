#!/usr/bin/env bash
# The single command an agent (or human) runs before cutting a katamari
# release to prove, from a pristine environment, that the whole product
# works end to end: the three cargo gates (plus the tmux smoke test when
# available), then a real `ktmr lsp install`/`ktmr doctor --json` pass
# against a throwaway multi-language monorepo — the same "does LSP actually
# come up in a fresh clone" question `ktmr doctor` exists to answer, just
# automated and asserted on instead of read by a human.
#
# Usage: scripts/release-check.sh [--full] [--langs=a,b,c] [--skip-gates] [--keep]
#   mise run release-check / mise run release-check-full are the two
#   canonical entry points; see AGENTS.md's "Release check" section for
#   which one to reach for and how long each takes. Runnable directly too,
#   from the repo root or anywhere else — same "resolve your own root"
#   trick as scripts/e2e-tmux.sh.
#
#   --full          Select all six languages (rust, typescript, python, go,
#                    kotlin, java) instead of the default three, provisioning
#                    a JDK/go toolchain via mise if neither is already
#                    reachable. Needs network for the extra downloads.
#   --langs=LIST     Comma-separated language list, overriding the default
#                    (rust,typescript,python) and --full's all-six default.
#                    Also settable via $RELEASE_CHECK_LANGS. A language named
#                    explicitly here is a hard requirement: if its toolchain
#                    prerequisite (go, a JDK) isn't reachable and can't be
#                    provisioned, the run fails fast rather than silently
#                    dropping it.
#   --skip-gates     Skip cargo test/clippy/fmt/e2e-tmux — just the
#                    monorepo/LSP/doctor pass, for a fast LSP-only run.
#   --keep           Don't delete the sandbox on exit; print its path
#                    instead, for debugging a failure.
#
# PRISTINE BY CONSTRUCTION: every `ktmr` invocation below runs with
# $HOME/$XDG_CONFIG_HOME/$XDG_DATA_HOME/$XDG_STATE_HOME pointed at a fresh
# tempdir — never the real ones. `lsp::install::prefix_dir` (the LSP install
# prefix) and `update::state_dir` (jdtls per-workspace index, `ktmr doctor`
# nothing else) both resolve purely from those four vars (see their doc
# comments), so this is sufficient to guarantee no run ever touches the
# invoking user's real `~/.config/katamari`, `~/.local/share/katamari`, or
# `~/.local/state/katamari` — same isolation `tests/support::Harness` and
# `e2e-tmux.sh` already rely on for the exact same reason. `PATH` gets the
# same treatment (see `SANDBOX_MINIMAL_PATH`'s definition, near `run_ktmr`,
# for the full why): isolating HOME/XDG_* alone still leaves every `ktmr`
# child resolving language servers through whatever happens to be on the
# invoking shell's ambient `PATH`, which can — and, on a machine with an
# ordinary rustup install, does — quietly preempt the sandbox's own
# managed install.
#
# Exits nonzero on the first thing that fails; the final summary lists every
# gate and language attempted either way, so a failure partway through still
# tells you what already passed.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# mise isn't reliably on PATH in every shell this might be launched from
# (see AGENTS.md's "Build/test" section) — added defensively, never
# overriding an existing PATH entry for it.
if ! command -v mise >/dev/null 2>&1; then
    export PATH="$HOME/.local/bin:$PATH"
fi
if ! command -v mise >/dev/null 2>&1; then
    echo "release-check: mise not found on PATH (even after adding ~/.local/bin)" >&2
    exit 1
fi

# --- args --------------------------------------------------------------

FULL=0
SKIP_GATES=0
KEEP=0
LANGS_ARG=""

for arg in "$@"; do
    case "$arg" in
        --full) FULL=1 ;;
        --skip-gates) SKIP_GATES=1 ;;
        --keep) KEEP=1 ;;
        --langs=*) LANGS_ARG="${arg#--langs=}" ;;
        -h | --help)
            sed -n '2,33p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "release-check: unrecognized argument: $arg (see --help)" >&2
            exit 1
            ;;
    esac
done

readonly ALL_SIX="rust,typescript,python,go,kotlin,java"
readonly DEFAULT_THREE="rust,typescript,python"

if [[ -n "$LANGS_ARG" ]]; then
    RAW_LANGS="$LANGS_ARG"
    LANGS_EXPLICIT=1
elif [[ -n "${RELEASE_CHECK_LANGS:-}" ]]; then
    RAW_LANGS="$RELEASE_CHECK_LANGS"
    LANGS_EXPLICIT=1
elif [[ "$FULL" -eq 1 ]]; then
    RAW_LANGS="$ALL_SIX"
    LANGS_EXPLICIT=0
else
    RAW_LANGS="$DEFAULT_THREE"
    LANGS_EXPLICIT=0
fi

IFS=',' read -ra RAW_LANG_LIST <<<"$RAW_LANGS"
REQUESTED_LANGS=()
for lang in "${RAW_LANG_LIST[@]}"; do
    lang="${lang//[[:space:]]/}" # tolerate "rust, typescript, python"
    [[ -z "$lang" ]] && continue # tolerate a trailing comma
    case "$lang" in
        rust | typescript | python | go | kotlin | java) ;;
        *)
            echo "release-check: unknown language '$lang' (want rust/typescript/python/go/kotlin/java)" >&2
            exit 1
            ;;
    esac
    REQUESTED_LANGS+=("$lang")
done
if [[ ${#REQUESTED_LANGS[@]} -eq 0 ]]; then
    echo "release-check: no languages given" >&2
    exit 1
fi

# --- output helpers ------------------------------------------------------

STEP_NAMES=()
STEP_RESULTS=()
STEP_SECONDS=()

now() { date '+%Y-%m-%d %H:%M:%S'; }

# Prints a step header with a timestamp, runs "$@", records pass/fail and
# elapsed time for the final summary, and re-raises the failure (via `exit`,
# `set -e` already active) rather than swallowing it — a release check that
# silently continued past a failed step wouldn't be one.
step() {
    local name="$1"
    shift
    echo
    echo "=== [$(now)] $name ==="
    local start end elapsed
    start=$(date +%s)
    # `( set -e; "$@" )`, not a bare `"$@"`: this whole call sits in an `if`
    # condition, and bash suspends `errexit` for the entire duration of any
    # compound command/function invoked directly as one (see bash(1) on
    # `set -e`) — without the subshell, a multi-command step function like
    # `build_monorepo_fixture` would only ever be judged on its *last*
    # command's exit status, silently ignoring an earlier failure instead of
    # aborting at it.
    if (set -e; "$@"); then
        end=$(date +%s)
        elapsed=$((end - start))
        echo "--- [$(now)] $name: ok (${elapsed}s) ---"
        STEP_NAMES+=("$name")
        STEP_RESULTS+=("PASS")
        STEP_SECONDS+=("$elapsed")
    else
        end=$(date +%s)
        elapsed=$((end - start))
        echo "--- [$(now)] $name: FAILED (${elapsed}s) ---" >&2
        STEP_NAMES+=("$name")
        STEP_RESULTS+=("FAIL")
        STEP_SECONDS+=("$elapsed")
        print_summary
        exit 1
    fi
}

# Same bookkeeping as `step`, for a language that was requested but whose
# toolchain prerequisite wasn't reachable and wasn't explicitly required
# (see the language-selection docs above `select_languages`) — recorded so
# the summary still names it, distinctly from a PASS or a FAIL.
record_skip() {
    local name="$1" reason="$2"
    echo
    echo "=== [$(now)] $name: SKIPPED — $reason ==="
    STEP_NAMES+=("$name")
    STEP_RESULTS+=("SKIP")
    STEP_SECONDS+=("0")
}

print_summary() {
    echo
    echo "=== release-check summary ==="
    local i
    for i in "${!STEP_NAMES[@]}"; do
        printf '  %-6s %-38s %ss\n' "${STEP_RESULTS[$i]}" "${STEP_NAMES[$i]}" "${STEP_SECONDS[$i]}"
    done
}

# --- sandbox: pristine by construction ------------------------------------

# The sandbox holds real language-server installs — a --full run peaks past
# 2GB (bootstrapped Node, jdtls, a compiled gopls, and kotlin-lsp's ~1GB
# extraction) — so it must live on a disk-backed filesystem. /tmp (and
# TMPDIR) is tmpfs on most modern Linux, where a full run dies mid-extract
# with a misleading "failed to unpack <some jar>" once the RAM-backed fs
# fills; /var/tmp is the FHS-designated disk-backed temp. Deliberately NOT
# honoring TMPDIR here for that reason — override with RELEASE_CHECK_TMPDIR
# if /var/tmp is unsuitable.
SANDBOX_PARENT="${RELEASE_CHECK_TMPDIR:-/var/tmp}"
SANDBOX="$(mktemp -d "$SANDBOX_PARENT/katamari-release-check.XXXXXX")"
avail_kb="$(df -Pk "$SANDBOX" | awk 'NR==2 {print $4}')"
need_kb=$((1024 * 1024)) # 1GB for the default language set
if [[ "$FULL" -eq 1 ]]; then
    need_kb=$((4 * 1024 * 1024)) # 4GB headroom for --full's server payloads
fi
if [[ -n "$avail_kb" && "$avail_kb" -lt "$need_kb" ]]; then
    echo "release-check: warning: only $((avail_kb / 1024))MB free under $SANDBOX_PARENT" \
        "(want $((need_kb / 1024))MB) — a mid-extraction unpack failure likely means disk," \
        "not a bad archive; set RELEASE_CHECK_TMPDIR to a roomier filesystem" >&2
fi
SANDBOX_HOME="$SANDBOX/home"
SANDBOX_CONFIG="$SANDBOX/home/config"
SANDBOX_DATA="$SANDBOX/home/data"
SANDBOX_STATE="$SANDBOX/home/state"
MONOREPO="$SANDBOX/monorepo"
mkdir -p "$SANDBOX_HOME" "$SANDBOX_CONFIG" "$SANDBOX_DATA" "$SANDBOX_STATE" "$MONOREPO"

cleanup() {
    if [[ "$KEEP" -eq 1 ]]; then
        echo
        echo "release-check: --keep passed, sandbox left at: $SANDBOX"
        return
    fi
    # Belt-and-braces beyond the orphan check below: kotlin-lsp resolves a
    # Gradle project by shelling out to Gradle, which — by Gradle's own
    # design, nothing katamari controls — detaches a long-lived daemon that
    # outlives kotlin-lsp itself and keeps files open under the sandbox's
    # kotlin-lsp install dir (observed directly: a `rm -rf` here failed with
    # "Directory not empty" while that daemon was still alive). Anything
    # still holding a path under `$SANDBOX` open at this point, named LSP
    # server or not, has no business surviving the run — matched generically
    # by command-line substring rather than by a second, Gradle-specific
    # process name to hunt for.
    pkill -9 -f -- "$SANDBOX" >/dev/null 2>&1 || true
    rm -rf "$SANDBOX"
}
trap cleanup EXIT

echo "release-check: sandbox at $SANDBOX"
echo "release-check: repo root $REPO_ROOT"
echo "release-check: languages requested: ${REQUESTED_LANGS[*]} (explicit: $([[ $LANGS_EXPLICIT -eq 1 ]] && echo yes || echo no))"

# --- language prerequisite resolution -------------------------------------
#
# Every lookup here runs against the REAL ambient environment (mise's own
# data dir, whatever's on the real PATH) — deliberately before any `ktmr`
# invocation switches HOME/XDG_* to the sandbox, since `mise where <tool>`
# resolves relative to mise's own data directory, which lives under the
# real $HOME. What a matched prerequisite produces (a directory to add to
# PATH, a JAVA_HOME) is plain data threaded into the later `ktmr`
# invocations via KTMR_EXTRA_PATH/KTMR_JAVA_HOME — never by exporting HOME
# itself early, which would make mise's own lookups start missing the
# real cache.

KTMR_EXTRA_PATH=""
KTMR_JAVA_HOME=""
SELECTED_LANGS=()

# `<install-dir>/bin/go`, from a toolchain already on PATH or one `mise`
# has cached under its pinned version — no provisioning. Prints the bin
# directory to add to PATH on success; empty output + nonzero exit if
# neither is found.
resolve_go_bin_dir() {
    if command -v go >/dev/null 2>&1; then
        dirname "$(command -v go)"
        return 0
    fi
    if command -v mise >/dev/null 2>&1; then
        local install_dir
        install_dir="$(mise where go@1.24 2>/dev/null || true)"
        if [[ -n "$install_dir" && -x "$install_dir/bin/go" ]]; then
            echo "$install_dir/bin"
            return 0
        fi
    fi
    return 1
}

# `$JAVA_HOME` if it's already set and points at a real JDK, else
# `mise where java@21`'s install dir if mise has one cached — no
# provisioning. Prints the JDK home directory on success.
resolve_java_home() {
    if [[ -n "${JAVA_HOME:-}" && -x "${JAVA_HOME}/bin/java" ]]; then
        echo "$JAVA_HOME"
        return 0
    fi
    if command -v mise >/dev/null 2>&1; then
        local install_dir
        install_dir="$(mise where java@21 2>/dev/null || true)"
        if [[ -n "$install_dir" && -x "$install_dir/bin/java" ]]; then
            echo "$install_dir"
            return 0
        fi
    fi
    return 1
}

# Resolves every requested language's toolchain prerequisite, populating
# SELECTED_LANGS (what actually runs), KTMR_EXTRA_PATH, and KTMR_JAVA_HOME.
# rust/typescript/python/kotlin never need anything from here (see the
# module doc comment on why: fully self-bootstrapping, or — for kotlin —
# no external prerequisite at all). go/java are gated: reachable already,
# reachable after `--full` provisions it via mise, or (an explicitly named
# `--langs`/`$RELEASE_CHECK_LANGS` entry only) a hard failure — see the
# script's top-of-file docs on why an explicit request never degrades to a
# silent skip the way the default/`--full` set does.
select_languages() {
    local lang
    for lang in "${REQUESTED_LANGS[@]}"; do
        case "$lang" in
            rust | typescript | python | kotlin)
                SELECTED_LANGS+=("$lang")
                ;;
            go)
                local bin_dir=""
                if bin_dir="$(resolve_go_bin_dir)"; then
                    :
                elif [[ "$FULL" -eq 1 ]]; then
                    echo "release-check: no go toolchain reachable, provisioning via 'mise x go@1.24 -- true'"
                    # `|| true`: an offline/failed provision must fall through
                    # to the reachability check just below (record_skip, or a
                    # hard failure if explicitly requested), not kill the
                    # whole script here via `set -e` on a bare failed command.
                    mise x go@1.24 -- true || true
                    bin_dir="$(resolve_go_bin_dir || true)"
                fi
                if [[ -n "$bin_dir" ]]; then
                    KTMR_EXTRA_PATH="$bin_dir:$KTMR_EXTRA_PATH"
                    SELECTED_LANGS+=("go")
                elif [[ "$LANGS_EXPLICIT" -eq 1 ]]; then
                    echo "release-check: go was explicitly requested but no go toolchain is reachable" \
                        "on PATH or via mise, and provisioning was not attempted or failed" \
                        "(pass --full to auto-provision via mise, or install go)" >&2
                    exit 1
                else
                    record_skip "lang: go" "no go toolchain reachable (pass --full to provision via mise)"
                fi
                ;;
            java)
                local jdk_home=""
                if jdk_home="$(resolve_java_home)"; then
                    :
                elif [[ "$FULL" -eq 1 ]]; then
                    echo "release-check: no JDK reachable, provisioning via 'mise x java@21 -- true'"
                    mise x java@21 -- true || true # see the go case's identical comment above
                    jdk_home="$(resolve_java_home || true)"
                fi
                if [[ -n "$jdk_home" ]]; then
                    KTMR_JAVA_HOME="$jdk_home"
                    SELECTED_LANGS+=("java")
                elif [[ "$LANGS_EXPLICIT" -eq 1 ]]; then
                    echo "release-check: java was explicitly requested but no JDK is reachable" \
                        "via \$JAVA_HOME or mise, and provisioning was not attempted or failed" \
                        "(pass --full to auto-provision via mise, or install a JDK)" >&2
                    exit 1
                else
                    record_skip "lang: java" "no JDK reachable (pass --full to provision via mise)"
                fi
                ;;
        esac
    done
}

select_languages
if [[ ${#SELECTED_LANGS[@]} -eq 0 ]]; then
    echo "release-check: no languages selected (all requested languages were skipped) — nothing to verify" >&2
    exit 1
fi
echo "release-check: languages selected: ${SELECTED_LANGS[*]}"

# `ktmr`'s own language-server resolution (adapter.rs's `lookup_in_order`)
# checks, in this exact order: a project-local convention, then `PATH`,
# then a toolchain-specific `which` (`rustup which` for rust-analyzer),
# then `mise which`, and *only last* whatever katamari itself already
# installed into the sandbox (`XDG_DATA_HOME`, isolated above). `PATH`
# sits at tier 2 — ahead of the sandbox's own tier 5 — so merely isolating
# HOME/XDG_* is not enough: anything of the right name sitting anywhere on
# the *ambient* `PATH` this script inherited still wins, sandbox or not.
#
# Verified in the field: `mise run release-check` activates this repo's
# `[tools] rust` (see mise.toml), which prepends rustup's real
# `~/.cargo/bin` to this very shell's `PATH` — and rustup's
# `rust-analyzer` proxy shim lives there too, *unconditionally*, whether or
# not the `rust-analyzer` component is actually installed (ordinary rustup
# state on plenty of machines, this one included). `which_on_path` only
# checks the shim is a file, not that running it works — so tier 2 reports
# a hit, resolution stops right there, and the sandbox's own
# `ktmr lsp install rust` copy (tier 5) never even gets asked. Direct
# invocation (no `mise run` prepending `~/.cargo/bin`) doesn't hit this,
# which is exactly why it passed while `mise run release-check` didn't.
#
# The fix: build every `ktmr` child's `PATH` from scratch, out of system
# directories only, plus exactly the toolchain directories THIS SCRIPT
# resolved/provisioned itself (today: the go bin dir, via
# `KTMR_EXTRA_PATH`, when go was selected — see `select_languages`; java
# is threaded through via `$JAVA_HOME` below instead, never `PATH`).
# `getconf PATH` reports the minimal `PATH` POSIX guarantees will find the
# platform's standard utilities — precisely the "sane baseline, nothing
# ambient" this needs — with a literal fallback for the (unlikely) case
# `getconf` itself isn't available. Neither `mise` nor `rustup` is ever on
# this `PATH`, on purpose: with them unreachable, adapter.rs's own
# `rustup_which`/`mise_which` tiers fail outright (`Command::new` can't
# even find the binary to shell out to) instead of forwarding some other
# ambient answer, so resolution has nowhere left to land but tier 5 — the
# sandbox. The two things a `ktmr` child needs that aren't "a system dir +
# go's bin dir" are `git` (a real system package, already covered by
# `getconf PATH`) and Node.js for the npm-strategy servers
# (typescript-language-server, pyright) — and that one's a non-issue by
# construction: `install::resolve_npm` only reaches for `PATH`/`mise`
# first as an optimization, and falls straight through to bootstrapping
# its own private Node.js straight into the sandbox the moment neither is
# found (verified directly: `ktmr lsp install typescript`/`python` both
# succeed under exactly this sanitized `PATH`, with no ambient `node`/`npm`
# on it at all).
SANDBOX_MINIMAL_PATH="$(getconf PATH 2>/dev/null || true)"
if [[ -z "$SANDBOX_MINIMAL_PATH" ]]; then
    SANDBOX_MINIMAL_PATH="/usr/local/bin:/usr/bin:/bin"
fi

# Every `ktmr` invocation below goes through this — the one place HOME/XDG_*
# get pointed at the sandbox (see the pristine-by-construction doc comment
# above), and the one place KTMR_EXTRA_PATH/SANDBOX_MINIMAL_PATH/
# KTMR_JAVA_HOME actually reach the process.
run_ktmr() {
    env \
        HOME="$SANDBOX_HOME" \
        XDG_CONFIG_HOME="$SANDBOX_CONFIG" \
        XDG_DATA_HOME="$SANDBOX_DATA" \
        XDG_STATE_HOME="$SANDBOX_STATE" \
        PATH="${KTMR_EXTRA_PATH}${SANDBOX_MINIMAL_PATH}" \
        JAVA_HOME="$KTMR_JAVA_HOME" \
        "$KTMR_BIN" "$@"
}

# Runs "$@" with $REPO_ROOT as its cwd, in a subshell so the `cd` never
# leaks into the rest of the script — every gate/build step below goes
# through this instead of a `bash -c '...'` string, which would need its
# own quoting pass and wouldn't inherit this shell's functions/PATH tweaks.
in_repo_root() {
    (cd "$REPO_ROOT" && "$@")
}

# --- gates -----------------------------------------------------------------

if [[ "$SKIP_GATES" -eq 1 ]]; then
    echo
    echo "release-check: --skip-gates passed, skipping cargo test/clippy/fmt/e2e-tmux"
else
    step "gate: cargo test" in_repo_root mise exec -- cargo test
    step "gate: cargo clippy" in_repo_root mise exec -- cargo clippy --all-targets -- -D warnings
    step "gate: cargo fmt --check" in_repo_root mise exec -- cargo fmt --check
    if command -v tmux >/dev/null 2>&1; then
        step "gate: e2e-tmux" in_repo_root mise run e2e-tmux
    else
        record_skip "gate: e2e-tmux" "tmux not found on PATH"
    fi
fi

# --- build the release binary under test ----------------------------------
#
# A dedicated build rather than reusing whatever `cargo test` produced: the
# gates above (when they run at all) build a *debug* `ktmr` as a side effect
# of `cargo test`/`e2e-tmux`, but a release check should exercise the same
# release-profile binary a real release actually ships (see `[profile.release]`
# in Cargo.toml) — and this step must work on its own regardless, since
# `--skip-gates` may have skipped every step that would otherwise have built
# anything at all.
step "build: cargo build --release --bin ktmr" in_repo_root mise exec -- cargo build --release --bin ktmr
KTMR_BIN="$REPO_ROOT/target/release/ktmr"
if [[ ! -x "$KTMR_BIN" ]]; then
    echo "release-check: expected a built binary at $KTMR_BIN" >&2
    exit 1
fi

# --- monorepo fixture --------------------------------------------------
#
# One throwaway git repo, six services, one per language katamari supports
# — always all six regardless of which languages are actually selected for
# the install/doctor pass below, so `ktmr doctor`'s resolution/live-probe
# sections always see the full monorepo shape a real polyglot repo would
# have (an unselected language's files simply show up as "present but
# unresolved" warnings, which the doctor-check step below already knows to
# ignore — see its docs).
build_monorepo_fixture() {
    local root="$MONOREPO"

    mkdir -p "$root/rustsvc/src"
    cat >"$root/rustsvc/Cargo.toml" <<'EOF'
[package]
name = "rustsvc"
version = "0.1.0"
edition = "2021"
EOF
    cat >"$root/rustsvc/src/main.rs" <<'EOF'
fn main() {
    println!("Hello from rustsvc");
}
EOF

    mkdir -p "$root/web"
    cat >"$root/web/package.json" <<'EOF'
{
  "name": "web",
  "version": "1.0.0",
  "private": true
}
EOF
    cat >"$root/web/tsconfig.json" <<'EOF'
{
  "compilerOptions": {
    "target": "es2020",
    "module": "commonjs",
    "strict": true
  }
}
EOF
    cat >"$root/web/index.ts" <<'EOF'
export function greet(name: string): string {
    return `Hello, ${name}!`;
}
EOF

    mkdir -p "$root/py"
    cat >"$root/py/app.py" <<'EOF'
def greet(name: str) -> str:
    return f"Hello, {name}!"


if __name__ == "__main__":
    print(greet("world"))
EOF

    mkdir -p "$root/gosvc"
    cat >"$root/gosvc/go.mod" <<'EOF'
module gosvc

go 1.24
EOF
    cat >"$root/gosvc/main.go" <<'EOF'
package main

import "fmt"

func main() {
    fmt.Println("Hello from gosvc")
}
EOF

    mkdir -p "$root/jsvc/src/main/java/com/example"
    cat >"$root/jsvc/pom.xml" <<'EOF'
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <modelVersion>4.0.0</modelVersion>
  <groupId>com.example</groupId>
  <artifactId>jsvc</artifactId>
  <version>1.0.0</version>
  <properties>
    <maven.compiler.source>21</maven.compiler.source>
    <maven.compiler.target>21</maven.compiler.target>
  </properties>
</project>
EOF
    cat >"$root/jsvc/src/main/java/com/example/App.java" <<'EOF'
package com.example;

public class App {
    public static void main(String[] args) {
        System.out.println("Hello from jsvc");
    }
}
EOF

    mkdir -p "$root/ksvc/src/main/kotlin/com/example"
    cat >"$root/ksvc/build.gradle.kts" <<'EOF'
plugins {
    kotlin("jvm") version "2.0.0"
}

repositories {
    mavenCentral()
}
EOF
    cat >"$root/ksvc/src/main/kotlin/com/example/App.kt" <<'EOF'
package com.example

fun main() {
    println("Hello from ksvc")
}
EOF

    git -C "$root" init -q
    git -C "$root" -c user.email="release-check@katamari.test" -c user.name="katamari release-check" \
        add -A
    git -C "$root" -c user.email="release-check@katamari.test" -c user.name="katamari release-check" \
        commit -q -m "throwaway monorepo fixture"
}

step "monorepo: build fixture" build_monorepo_fixture

# Wraps `run_ktmr` in a subshell that `cd`s into the monorepo fixture first
# — a plain `(...)` subshell rather than `bash -c '...'`, so it still
# inherits `run_ktmr` and every sandbox variable this script already
# defined, instead of needing them re-exported into a fresh shell. Only
# `ktmr doctor` actually needs to run from inside the fixture (it discovers
# the repo from `$PWD`); `ktmr lsp install` doesn't care where it's run
# from, but routing it through here too keeps every `ktmr` call site
# uniform.
run_ktmr_in_monorepo() {
    (cd "$MONOREPO" && run_ktmr "$@")
}

# --- language server installs -----------------------------------------
#
# Deliberately `ktmr lsp install`, not letting the doctor pass below
# trigger auto-install lazily — installing explicitly here is what gives
# each language's install its own timed, individually-failing step, rather
# than folding every language's install time into one opaque `doctor` call
# and only finding out which one failed from its JSON. `run_lsp_install` in
# main.rs `anyhow::bail!`s (nonzero exit) if the one language it was asked
# to install fails, specifically so a script like this one sees it as a
# real failure rather than a line of prose it has to go parsing for — a
# plain `step` call is all that's needed here.

for lang in "${SELECTED_LANGS[@]}"; do
    step "lsp install: $lang" run_ktmr_in_monorepo lsp install "$lang"
done

# --- doctor: one pass over the whole monorepo ---------------------------
#
# Exactly one `ktmr doctor --json` call, in the monorepo fixture, after
# every selected language's server is already installed — see the module
# docs above `build_monorepo_fixture` for why every language's files are
# present regardless of selection, and `check_doctor_report.py` for how the
# unselected ones' warnings are filtered out rather than failing the run.
DOCTOR_JSON="$SANDBOX/doctor.json"

# `ktmr doctor` exits nonzero the instant ANY check anywhere in the report
# is error-level (see `run_doctor`/`doctor::exit_code` in main.rs/doctor.rs)
# — and "anywhere" really does mean anywhere: build_monorepo_fixture always
# lays down files for all six languages regardless of selection, so an
# unselected language's server (never installed via `ktmr lsp install` this
# run, and possibly broken/ambient-something on the machine besides) can
# fail its live probe and flip that exit code, even though every SELECTED
# language is perfectly healthy. Gating this step on that exit code — the
# original approach here — fails the whole run over a language nobody
# asked to check, defeating `--langs`/`$RELEASE_CHECK_LANGS` entirely
# (verified: a `RELEASE_CHECK_LANGS=python` run failed here on an ambient,
# unselected rust-analyzer breakage even though every python check was
# `ok`). So this step treats that exit code as pure information — captured
# and logged, never fatal — and only `check_doctor_report` below, which
# looks solely at SELECTED_LANGS' own live-probe checks (and treats an
# unselected language's warnings *and* errors alike as informational; see
# its docstring), is the actual gate. The full human-readable rendering
# still comes from that same step, via the JSON this one wrote.
run_doctor_json() {
    local rc=0
    run_ktmr_in_monorepo doctor --json >"$DOCTOR_JSON" || rc=$?
    if [[ "$rc" -ne 0 ]]; then
        echo "release-check: ktmr doctor exited $rc — informational only (see this step's" \
            "comment above); the selected-language live-probe check below is the real gate"
    fi
    return 0
}
step "doctor: ktmr doctor --json" run_doctor_json

CHECK_SCRIPT="$SANDBOX/check_doctor_report.py"
cat >"$CHECK_SCRIPT" <<'PYEOF'
"""Renders `ktmr doctor --json`'s report as human-readable text (mirroring
doctor::render_text's shape closely enough to be recognizable, without
re-implementing it) and decides pass/fail for release-check's purposes:
every SELECTED language's "lsp (live probe)" checks — spawn+initialize and
hover round-trip, or the single unresolved/no-probe-file skip note in their
place — must all be `ok`. A check belongs to language L when its label is
exactly L (a skip note) or starts with "L:" (spawn+initialize/hover round-
trip) — see doctor.rs's `key_label`/`classify_spawn_outcome`/
`classify_hover_outcome` for where those labels come from. Checks for a
language that ISN'T selected (the monorepo fixture always has files for
all six, see build_monorepo_fixture's docs) are rendered but never gate the
run — an unselected language's server was never installed, so an
"unresolved, skipped" warning for it is normal and expected, not a
regression. A selected language with zero matching checks at all (its file
went missing, or detection somehow didn't route it) is its own failure —
silence isn't the same as `ok`.
"""
import json
import sys

def main() -> int:
    json_path = sys.argv[1]
    selected = sys.argv[2:]

    with open(json_path, encoding="utf-8") as f:
        report = json.load(f)

    for section in report["sections"]:
        print(section["title"])
        for check in section["checks"]:
            tag = check["status"]
            label = check["label"]
            detail = check["detail"]
            line = f"  {tag:<5} {label}"
            if detail:
                line += f": {detail}"
            print(line)
        print()

    live_checks = next(
        (s["checks"] for s in report["sections"] if s["title"] == "lsp (live probe)"),
        [],
    )

    seen = {lang: False for lang in selected}
    failures = []
    for check in live_checks:
        lang = check["label"].split(":", 1)[0].strip()
        if lang not in seen:
            continue
        seen[lang] = True
        if check["status"] != "ok":
            failures.append(
                f'{lang}: "{check["label"]}" is {check["status"]} — {check["detail"]}'
            )

    for lang, was_seen in seen.items():
        if not was_seen:
            failures.append(
                f"{lang}: no \"lsp (live probe)\" check found at all "
                "(expected a spawn+initialize/hover round-trip pair, or a skip note)"
            )

    if failures:
        print("release-check: doctor live-probe check FAILED for selected languages:")
        for failure in failures:
            print(f"  - {failure}")
        return 1

    print(f"release-check: doctor live-probe checks all ok for: {', '.join(selected)}")
    return 0

if __name__ == "__main__":
    sys.exit(main())
PYEOF

check_doctor_report() {
    python3 "$CHECK_SCRIPT" "$DOCTOR_JSON" "${SELECTED_LANGS[@]}"
}
step "doctor: evaluate selected-language live-probe checks" check_doctor_report

# --- orphan check ---------------------------------------------------------
#
# `lsp_live_section` calls `manager.shutdown_all()` before `ktmr doctor`
# returns (see doctor.rs), which sends `shutdown`/`exit` and kills anything
# that doesn't exit within `Client::shutdown`'s own grace period — this is
# what actually proves that promise held, in the one process-table way a
# unit test can't reach. Matched by command-line substring (`pgrep -f`),
# since the npm-installed servers run as `node <script-path>` — their
# argv[0] is `node`, not the server name, so only the full command line
# (which still contains the install path, hence the server's directory
# name) reliably identifies them; jdtls is likewise a `java -jar
# .../plugins/org.eclipse.equinox.launcher_*.jar` invocation, matched on
# its launcher jar rather than on `java` itself (which would false-positive
# on any unrelated java process on the machine).
ORPHAN_PATTERN='rust-analyzer|typescript-language-server|pyright-langserver|/gopls|kotlin-lsp/.*intellij-server|org\.eclipse\.equinox\.launcher_'

check_no_orphan_servers() {
    local matches
    matches="$(pgrep -af -- "$ORPHAN_PATTERN" 2>/dev/null || true)"
    if [[ -n "$matches" ]]; then
        echo "release-check: found LSP server process(es) still running after doctor's shutdown_all:" >&2
        echo "$matches" >&2
        return 1
    fi
    echo "release-check: no orphaned LSP server processes found"
}
step "orphan check: no LSP servers left running" check_no_orphan_servers

# --- summary ---------------------------------------------------------------

print_summary
FAILED=0
for result in "${STEP_RESULTS[@]}"; do
    [[ "$result" == "FAIL" ]] && FAILED=1
done
if [[ "$FAILED" -eq 1 ]]; then
    echo
    echo "release-check: FAIL"
    exit 1
fi
echo
echo "release-check: PASS"
