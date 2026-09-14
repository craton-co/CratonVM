#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# check-probes.sh — every probe in this directory must COMPILE.
#
# WHY THIS EXISTS
# ---------------
# Probes are not in the suite's `CLASSES` list, so nothing ever compiles them
# and a broken one is discovered by the next person who tries to run it — often
# days later, in the middle of a measurement.
#
# FOUR probes filed in one week did not compile, all for the same reason: the
# public class name did not match the file name. They were fixed by hand. This
# script found a FIFTH still broken at 22cb4338d —
# `Sweep5CollectionContracts.java` declaring `public class Sweep5`, which javac
# refuses outright — so hand-fixing was not converging.
#
# TWO CHECKS, and the second is not implied by the first
# ------------------------------------------------------
#   (1) NAME  the public type's name equals the file's basename. javac says this
#             too, but as one error among many, and this check names the fix.
#   (2) JAVAC every file compiles. A probe can have a matching name and still be
#             broken — a missing import, a bad lambda, an API that moved.
#
# It compiles the files ONE AT A TIME, not as one javac invocation. A batch
# compile lets a probe that references another probe's class pass by accident,
# and probes are meant to be self-contained: each is run as its own `-cp <dir>
# <Class>`.
#
# WHAT IT IS NOT: it does not RUN anything. A probe that compiles can still be
# confounded (`H0-8`) or blind (`H17`); those are `chm-consistency.sh` and
# `dispatch-witness.sh`.
#
# Usage: JDK=C:/path/to/jdk regression-suite/probes/check-probes.sh
#        regression-suite/probes/check-probes.sh --selftest
# Exit:  0 every probe compiles · 1 at least one does not · 3 could not run
# ---------------------------------------------------------------------------
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"

# The declared public type of a .java file, or empty. Kept as a function so the
# selftest can drive it over files it writes itself — a name check nobody has
# watched reject a bad name is decoration.
#
# `grep -a`, and it is not defensive tidiness. A .java file may legitimately
# contain a NUL byte: `regression-suite/probes/W4Data.java` writes
# `d.writeUTF("a\0b")` to exercise modified UTF-8, and `file(1)` calls it
# `data`. Without `-a`, GNU grep prints `Binary file (standard input) matches`
# INSTEAD OF the matched line, and this function returned that sentence as the
# declared type name — so the gate reported
#
#     NAME  W4Data.java declares 'public Binary file (standard input) matches'
#
# on a probe that compiles perfectly (`javac` rc=0). **A false NAME mismatch is
# worse than no check**: it blocks CI on a good file and teaches the reader to
# ignore the gate. `LC_ALL=C` for the same class of reason — a multi-byte
# locale can make `sed` fail on the same bytes.
public_type() {
  LC_ALL=C sed -E 's://.*::' "$1" \
    | LC_ALL=C grep -aoE '(^|[[:space:]])public[[:space:]]+((final|abstract|sealed|non-sealed|static)[[:space:]]+)*(class|interface|enum|record)[[:space:]]+[A-Za-z0-9_$]+' \
    | head -1 \
    | LC_ALL=C sed -E 's/.*(class|interface|enum|record)[[:space:]]+//'
}

if [ "${1:-}" = "--selftest" ]; then
  fails=0
  t=$(mktemp -d) || exit 3
  trap 'rm -rf "$t"' EXIT
  printf 'public class Good {}\n'                       > "$t/Good.java"
  printf '/** doc */\npublic final class Fin {}\n'      > "$t/Fin.java"
  printf 'public enum E { A }\n'                        > "$t/E.java"
  printf 'public record R(int x) {}\n'                  > "$t/R.java"
  printf 'public interface I {}\n'                      > "$t/I.java"
  printf '// public class Commented {}\npublic class Real {}\n' > "$t/Real.java"
  printf 'class NotPublic {}\n'                         > "$t/NotPublic.java"
  # The exact shape that broke Sweep5CollectionContracts.
  printf 'public class Sweep5 {}\n'                     > "$t/Sweep5CollectionContracts.java"
  # A NUL byte in a string literal is LEGAL and REAL: W4Data.java writes
  # `d.writeUTF("a\0b")` to exercise modified UTF-8, so `file(1)` calls that
  # probe `data`. Without `grep -a`, public_type() returned the literal string
  # 'Binary file (standard input) matches' as the declared type, and the gate
  # reported a NAME MISMATCH on a probe javac accepts. See public_type().
  printf 'public class Nul { String s = "a\000b"; }\n'   > "$t/Nul.java"

  for pair in "Good:Good" "Fin:Fin" "E:E" "R:R" "I:I" "Real:Real" \
              "NotPublic:" "Nul:Nul" "Sweep5CollectionContracts:Sweep5"; do
    f=${pair%%:*}; want=${pair#*:}
    got=$(public_type "$t/$f.java")
    if [ "$got" = "$want" ]; then echo "  ok   public_type($f.java) = '${want:-<none>}'"
    else echo "  FAIL public_type($f.java) = '$got', expected '${want:-<none>}'"; fails=1; fi
  done

  # The name check itself must REJECT the Sweep5 shape and ACCEPT the good one.
  if [ "$(public_type "$t/Sweep5CollectionContracts.java")" = "Sweep5CollectionContracts" ]; then
    echo "  FAIL the name check accepts the shape that broke four probes"; fails=1
  else
    echo "  ok   the name check rejects the shape that broke four probes"
  fi
  # A comment mentioning a class must not be mistaken for a declaration.
  [ "$(public_type "$t/Real.java")" = "Real" ] \
    && echo "  ok   a commented-out declaration is not read as the public type" \
    || { echo "  FAIL a comment was read as the declaration"; fails=1; }

  [ "$fails" -eq 0 ] && { echo "  selftest OK — six declaration shapes, the no-public-type case,"
                          echo "  the Sweep5 shape rejected, and a comment not mistaken for code"; exit 0; }
  exit 3
fi

JDK="${JDK:-${JAVA_HOME:-}}"
[ -n "$JDK" ] || { echo "ERROR: set JDK= (or JAVA_HOME); on MSYS use"
                   echo "  JDK=\$(cygpath -m \"\$(dirname \"\$(dirname \"\$(command -v javap)\")\")\")"
                   exit 3; }
JAVAC="$JDK/bin/javac.exe"
[ -x "$JAVAC" ] || JAVAC="$JDK/bin/javac"
[ -x "$JAVAC" ] || { echo "ERROR: no javac at $JAVAC"; exit 3; }

OUT=$(mktemp -d) || exit 3
trap 'rm -rf "$OUT"' EXIT
OUTW="$OUT"
command -v cygpath > /dev/null 2>&1 && OUTW=$(cygpath -m "$OUT")

n=0; badname=0; badjavac=0
for f in "$HERE"/*.java; do
  [ -e "$f" ] || continue
  n=$((n + 1))
  b=$(basename "$f" .java)
  pt=$(public_type "$f")
  if [ -n "$pt" ] && [ "$pt" != "$b" ]; then
    echo "  NAME  $b.java declares 'public $pt' — javac refuses this outright."
    echo "        Rename the type to $b (the file name is what records cite)."
    badname=$((badname + 1))
    continue
  fi
  fw="$f"
  command -v cygpath > /dev/null 2>&1 && fw=$(cygpath -m "$f")
  # One at a time on purpose: a batch compile lets a probe resolve another
  # probe's class, and every probe is run standalone.
  if ! err=$("$JAVAC" -nowarn -d "$OUTW" "$fw" 2>&1); then
    echo "  JAVAC $b.java does not compile:"
    printf '%s\n' "$err" | grep -vE '^Note:' | head -4 | sed 's/^/        /'
    badjavac=$((badjavac + 1))
  fi
done

[ "$n" -eq 0 ] && { echo "REFUSING: no .java found in $HERE. An empty sweep passes trivially."; exit 3; }
bad=$((badname + badjavac))
echo "PROBE CHECK: $n probe(s), $badname name mismatch(es), $badjavac compile failure(s)."
[ "$bad" -eq 0 ] && echo "  ok — every probe in this directory compiles standalone."
[ "$bad" -eq 0 ] || echo "  A probe nothing compiles is found broken by the next person to need it."
exit $((bad > 0 ? 1 : 0))
