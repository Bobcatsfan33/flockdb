#!/usr/bin/env bash
#
# Run the README's quickstart. Not a copy of it — *it*.
#
# ─────────────────────────────────────────────────────────────────────────────────────────────────
# WHY THIS SCRIPT EXISTS
#
# FlockDB's exit criterion for F2 is a sentence about a person: **a stranger forks a database within
# ninety seconds of opening the README**. That is a claim about a document, and a claim about a
# document rots the instant someone edits the code without editing the document. A CI job that ran a
# *paraphrase* of the quickstart would keep passing while the README told the stranger to type a flag
# that no longer exists.
#
# So this script does not contain the quickstart. It PARSES the quickstart out of README.md, between
# the QUICKSTART:BEGIN and QUICKSTART:END markers, runs each command, and asserts that the output is
# **byte-for-byte what the README claims it will be**. If you change the CLI's output, this fails
# until you change the README. If you change the README, this fails until the CLI agrees. There is no
# third state in which they can disagree and CI is green.
#
# It also times it, and fails if the five commands take longer than the budget — because a claim
# nobody measures is a wish.
#
#   usage:  scripts/quickstart.sh
#           FLOCK_BIN=target/release/flock scripts/quickstart.sh
#           FLOCK_QUICKSTART_BUDGET=90 scripts/quickstart.sh
#
# NOTE ON WHAT IS *NOT* TIMED: the build. `cargo install` compiles DuckDB from source and takes
# minutes, and no amount of shell can make that a lie. The README says so in the same breath as it
# claims the ninety seconds. See "How long this actually takes" there.
# ─────────────────────────────────────────────────────────────────────────────────────────────────

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
README="$REPO_ROOT/README.md"
BUDGET="${FLOCK_QUICKSTART_BUDGET:-90}"

# The binary under test. Default to a release build, because that is what `cargo install` produces
# and therefore what the stranger will actually run.
FLOCK_BIN="${FLOCK_BIN:-$REPO_ROOT/target/release/flock}"
# Resolve to an absolute path. The binary is symlinked onto PATH from a scratch dir below, and a
# relative FLOCK_BIN would produce a dangling symlink that fails as "command not found" — an error
# that looks like a broken CLI when it is really just a relative path. Do not make the caller guess.
case "$FLOCK_BIN" in
    /*) : ;;
    *) FLOCK_BIN="$PWD/$FLOCK_BIN" ;;
esac
if [[ ! -x "$FLOCK_BIN" ]]; then
    echo "error: no flock binary at $FLOCK_BIN" >&2
    echo "  build one:  cargo build --release -p flock-cli" >&2
    echo "  or point at one:  FLOCK_BIN=/path/to/flock $0" >&2
    exit 1
fi

# A scratch directory that looks like a fresh clone: the examples, and nothing else. `flock import`
# creates `.flock/` in the working directory, and running the quickstart in the repo itself would
# leave that behind — which would make the *second* run of this script a different test from the
# first, and a test that only passes once is not a test.
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cp -R "$REPO_ROOT/examples" "$WORK/examples"

# So that the README's literal `flock ...` resolves, whatever the binary is called or where it is.
mkdir -p "$WORK/bin"
ln -s "$FLOCK_BIN" "$WORK/bin/flock"
export PATH="$WORK/bin:$PATH"

cd "$WORK"

# ── Extract the quickstart from the README ───────────────────────────────────────────────────────
#
# Inside the marked block: a line starting with "$ " is a command; every other non-blank line is the
# output the README promises. Blank lines separate one command from the next.
BLOCK="$(awk '/<!-- QUICKSTART:BEGIN -->/{f=1;next} /<!-- QUICKSTART:END -->/{f=0} f' "$README" \
         | sed -e '/^```/d')"

if [[ -z "${BLOCK//[[:space:]]/}" ]]; then
    echo "error: found no quickstart in $README between <!-- QUICKSTART:BEGIN --> and <!-- QUICKSTART:END -->" >&2
    exit 1
fi

# Normalise for comparison: strip trailing whitespace, drop blank lines. Nothing else — the table
# borders, the row order and the numbers are all compared exactly, because all three are things a
# reader will believe.
normalise() { sed -e 's/[[:space:]]*$//' -e '/^$/d'; }

commands=0
failures=0
start="$(python3 -c 'import time; print(repr(time.time()))')"

pending_cmd=""
pending_expect=""

check() {
    [[ -z "$pending_cmd" ]] && return 0
    commands=$((commands + 1))

    echo "\$ $pending_cmd"
    local actual
    if ! actual="$(eval "$pending_cmd" 2>&1)"; then
        echo "$actual"
        echo "!! the README's command FAILED" >&2
        failures=$((failures + 1))
        pending_cmd=""; pending_expect=""
        return 0
    fi
    echo "$actual"

    local got want
    got="$(printf '%s\n' "$actual" | normalise)"
    want="$(printf '%s\n' "$pending_expect" | normalise)"

    if [[ "$got" != "$want" ]]; then
        echo "!! the README claims this command prints:" >&2
        printf '%s\n' "$want" | sed 's/^/     /' >&2
        echo "!! it actually printed:" >&2
        printf '%s\n' "$got" | sed 's/^/     /' >&2
        echo "!! README.md and the CLI disagree. Fix one of them." >&2
        failures=$((failures + 1))
    fi
    echo
    pending_cmd=""
    pending_expect=""
}

while IFS= read -r line; do
    if [[ "$line" == '$ '* ]]; then
        check
        pending_cmd="${line#\$ }"
    elif [[ -n "${line//[[:space:]]/}" ]]; then
        pending_expect+="$line"$'\n'
    fi
done <<< "$BLOCK"
check

elapsed="$(python3 -c "import time; print(f'{time.time() - $start:.2f}')")"

echo "─────────────────────────────────────────────────────────────"
echo "quickstart: $commands commands from README.md, $failures mismatched"
echo "QUICKSTART_ELAPSED_SECONDS=$elapsed   (budget: ${BUDGET}s)"
echo "─────────────────────────────────────────────────────────────"

if [[ "$commands" -eq 0 ]]; then
    echo "error: the quickstart block contained no commands" >&2
    exit 1
fi
if [[ "$failures" -ne 0 ]]; then
    exit 1
fi
if python3 -c "import sys; sys.exit(0 if $elapsed > $BUDGET else 1)"; then
    echo "error: the quickstart took ${elapsed}s, and the README promises under ${BUDGET}s." >&2
    echo "       Either it got slower, or the promise was never true. Do not edit the budget to" >&2
    echo "       make this pass." >&2
    exit 1
fi

echo "The README's quickstart runs, prints what it says it prints, and does it in ${elapsed}s."
