#!/usr/bin/env bash
# The regression suite's ENVIRONMENT-FAULT CLASSIFIER.
#
# WHY THIS FILE EXISTS
# --------------------
# `run.sh` reports a non-zero CratonVM exit as
#
#     FAIL  cratonvm rc=1
#
# unless its `sig` grep finds one of `AssertionError|NoSuchMethod|linkage
# error|panic|SEGV|fatal` in the output. Nothing in that alternation can match
# **a VM that died before it ever reached the vector**, so a broken JDK path, a
# rejected command line and a missing main class all print the same bare line a
# genuine assertion failure prints — and a lane reads it as a red vector.
#
# That is not hypothetical. It is `WORKER-5`'s trap 1, and it cost a lane a day:
# the JDK-path recipe in the older briefs,
#
#     JDK="$(dirname "$(dirname "$(command -v javap)")")"
#
# yields the MSYS POSIX spelling; `run.sh` exports `MSYS_NO_PATHCONV=1`, so it
# reaches `cratonvm.exe` unconverted and the run dies before any vector does.
# MEASURED A/B: POSIX form 0 passed / 5 failed, Windows form 5 passed / 0 failed.
# Use `cygpath -m "$(dirname "$(dirname "$(command -v javap)")")"`.
#
# **WHERE it dies was itself wrong until 2026-08-21, and this file must not
# repeat the error.** `H24-2`, H0 and the first version of THIS file all said
# "the VM dies in argument parsing". It does not: the VM ACCEPTS the POSIX
# spelling on the command line and dies **validating the JDK image** —
# `Provide a valid JDK installation (must contain 'jmods/' or 'lib/modules')`.
# Nobody could see that because the harness printed a bare `cratonvm rc=1`, and
# the VM's own banner says `failure occurred during argument parsing`, which is
# where three readers got it from. The banner is the KEY this file greps for; it
# is not a description to be quoted.
#
# WHAT THE VM ACTUALLY PRINTS — measured 2026-08-21 against cratonvm-r10.exe
# --------------------------------------------------------------------------
# The discriminator already exists in the VM's own output and nothing read it:
#
#   bad --java-home       rc=1  `[cratonvm] main-vm run() returned Err: --java-home
#                               path does not exist or is not a directory: …`
#                         and   `[cratonvm] jdk mode: <not yet resolved — failure
#                               occurred during argument parsing>`
#   unknown flag          rc=2  `error: unexpected argument '--no-such-flag' found`
#                               `Usage: cratonvm-r10.exe --java-home <PATH> …`
#   flag missing a value  rc=2  `error: a value is required for '--java-home <PATH>'
#                               but none was supplied`
#   conflicting modes     rc=2  `error: the argument '--jdk-only' cannot be used
#                               with '--synthetic-jdk'`
#   missing main class    rc=1  `Could not find or load main class NoSuchMain`
#                         and   `[cratonvm] jdk mode: real-jdk (java.home=…)`
#
# The last pair is the control that makes the first one a signal: a VM that got
# far enough to resolve its JDK mode prints the mode, and a VM that did not
# prints `<not yet resolved`. So "could not parse its arguments" is a one-line
# grep, and it is the line `run.sh` never looked at.
#
# WHY A SEPARATE FILE
# -------------------
# `run.sh` and `harness-guard.sh` belong to H0. All the logic and all the tests
# live here, so the change to `run.sh` is three small hunks: one `.` beside the
# `harness-guard.sh` source, one `if` around the existing `sig` grep, and one
# summary block. They are reproduced in
# docs/known-issues/jdk-only/WORKER-5-NOTE-2-*.md so H0 can read or revert them
# without diffing. `git revert` of that commit plus deleting this file restores
# the previous behaviour exactly; nothing else in the suite depends on it.
#
# WHAT IT IS NOT
# --------------
# It does NOT decide PASS/FAIL and it does not suppress a failure. A vector
# whose VM never started is still red — the point is that its `why` says the
# ENVIRONMENT is broken rather than implying the VM computed a wrong answer.
# `harness-guard.sh`'s own note applies here too: "the VM answered wrongly" and
# "the instrument could not run" are different findings.
#
# Usage:  . regression-suite/harness-vmfault.sh
#         why=$(vm_fault_class "$cvrc" "$cvout") && echo "$why"
#         regression-suite/harness-vmfault.sh --selftest

# Classify a CratonVM run that never reached its vector.
#   $1  the exit code
#   $2  the combined stdout+stderr
# Prints a ONE-LINE reason and returns 0 when the run is an ENVIRONMENT fault;
# prints nothing and returns 1 when it is not, so the caller falls through to
# its own assertion-signature grep unchanged.
#
# ONE line, deliberately. A bad --java-home fails EVERY vector in the run, so a
# four-line explanation would print 105 times. The one line carries the fix
# inline and `vm_fault_hint` (below) carries the long form for the summary.
vm_fault_class() {
  _vmf_rc="$1"; _vmf_out="$2"

  # 1. The VM exited BEFORE it resolved its JDK mode. This is the trap-1 shape.
  #    Checked FIRST because such a run also carries an `Err:` line whose text
  #    varies, while this banner does not.
  #
  #    The banner reads `failure occurred during argument parsing`; do NOT quote
  #    it as the cause. The JDK-image sub-case below is the common one and is
  #    NOT an argument-parsing failure at all — see the header.
  case "$_vmf_out" in
    *'jdk mode: <not yet resolved'*)
      _vmf_why=$(printf '%s\n' "$_vmf_out" \
        | grep -aE 'main-vm run\(\) returned Err:' | head -1 \
        | sed 's/^\[cratonvm\] main-vm run() returned Err: //; s/\x1b\[[0-9;]*m//g' \
        | cut -c1-90)
      case "$_vmf_out" in
        *'Provide a valid JDK installation'*|*'--java-home path does not exist'*)
          printf 'rc=%s: HARNESS FAULT — VM REJECTED THE JDK IMAGE: %s [on MSYS use cygpath -m]\n' \
                 "$_vmf_rc" "${_vmf_why:-see the run() Err line}" ;;
        *)
          printf 'rc=%s: HARNESS FAULT — VM EXITED BEFORE RESOLVING ITS JDK MODE (launch/config): %s\n' \
                 "$_vmf_rc" "${_vmf_why:-see the run() Err line}" ;;
      esac
      return 0 ;;
  esac

  # 2. The command line was rejected by the argument parser itself. rc=2 alone
  #    is not enough (a vector may exit 2), and `error:` alone is not enough (a
  #    vector may print it), so all three signals are required.
  if [ "$_vmf_rc" = 2 ] \
     && printf '%s\n' "$_vmf_out" | grep -qaE '^error: ' \
     && printf '%s\n' "$_vmf_out" | grep -qaE "^Usage: |try '--help'"; then
    _vmf_why=$(printf '%s\n' "$_vmf_out" | grep -aE '^error: ' | head -1 | cut -c1-90)
    printf 'rc=%s: HARNESS FAULT — VM REJECTED ITS COMMAND LINE: %s [check CRATONVM_ARGS]\n' \
           "$_vmf_rc" "$_vmf_why"
    return 0
  fi

  # 3. The VM started, resolved its mode, and could not find the class. That is
  #    a build/classpath fault in the harness, not an answer from the VM.
  case "$_vmf_out" in
    *'Could not find or load main class'*)
      _vmf_why=$(printf '%s\n' "$_vmf_out" \
        | grep -a 'Could not find or load main class' | head -1 \
        | sed 's/.*Could not find or load main class /main class /' | cut -c1-90)
      printf 'rc=%s: HARNESS FAULT — MAIN CLASS NOT FOUND: %s [javac output or -cp]\n' \
             "$_vmf_rc" "$_vmf_why"
      return 0 ;;
  esac

  # 4. `timeout` killed it. WORKER-5 trap 7: RMapGcStress needs 233 s against a
  #    120 s budget, so rc=124 on it is a BUDGET statement, not an objection to
  #    anyone's change. Use TIMEOUT=600.
  if [ "$_vmf_rc" = 124 ]; then
    printf 'rc=124: HARNESS FAULT — TIMED OUT; the harness killed the VM, it did not fail [try TIMEOUT=600]\n'
    return 0
  fi

  # 5. The binary is not there / not executable. Otherwise this reads as a VM
  #    that ran and failed instantly.
  if [ "$_vmf_rc" = 126 ] || [ "$_vmf_rc" = 127 ]; then
    printf 'rc=%s: HARNESS FAULT — CRATONVM COULD NOT BE EXECUTED; wrong $CV path, or not +x\n' "$_vmf_rc"
    return 0
  fi

  return 1
}

# The long form, for a summary. Takes a `why` produced by `vm_fault_class` and
# prints the paragraph that belongs at the END of a run, once — not beside each
# of the 105 vectors the same broken environment killed.
vm_fault_hint() {
  case "$1" in
    *'REJECTED THE JDK IMAGE'*)
      echo "  The VM never resolved its JDK mode, so it never ran a vector. It did NOT"
      echo "  fail to parse the command line — it accepted --java-home and then rejected"
      echo "  the image behind it. On MSYS/Git Bash the usual cause is a POSIX path:"
      echo "  run.sh exports MSYS_NO_PATHCONV=1, so it reaches cratonvm.exe unconverted."
      echo "  MEASURED A/B: POSIX 0 passed / 5 failed, Windows 5 passed / 0 failed. Use"
      echo '    JDK=$(cygpath -m "$(dirname "$(dirname "$(command -v javap)")")")' ;;
    *'BEFORE RESOLVING ITS JDK MODE'*)
      echo "  The VM exited during startup, before it resolved its JDK mode, so no vector"
      echo "  ran. The run() Err line above is the VM's own reason." ;;
    *'REJECTED ITS COMMAND LINE'*)
      echo "  A flag in CRATONVM_ARGS or in a per-class argument list is unknown to this"
      echo "  binary, is missing its value, or conflicts with another. Nothing ran." ;;
    *'MAIN CLASS NOT FOUND'*)
      echo "  The VM started and could not find the class: javac output, -cp, or the"
      echo "  module copy is wrong. This is a build fault, not a VM answer." ;;
    *'TIMED OUT'*)
      echo "  rc=124 is the harness's own timeout, not a VM failure. RMapGcStress needs"
      echo "  233 s against the default 120 s budget — re-run with TIMEOUT=600 before"
      echo "  reading a 124 as a regression." ;;
    *'COULD NOT BE EXECUTED'*)
      echo "  \$CV does not point at an executable cratonvm. Nothing ran." ;;
    *) return 1 ;;
  esac
  return 0
}

# ---------------------------------------------------------------------------
# SELFTEST. Every branch, and — the half that matters — every NEGATIVE control.
# A classifier that fires on a real assertion failure is worse than none: it
# would relabel the very failures the suite exists to report.
# ---------------------------------------------------------------------------
if [ "${1:-}" = "--selftest" ]; then
  _f=0
  _ok() { printf '  ok   %s\n' "$1"; }
  _no() { printf '  FAIL %s\n' "$1"; _f=1; }

  _pos() { # <label> <rc> <output> <expected-substring>
    _o=$(vm_fault_class "$2" "$3"); _r=$?
    if [ "$_r" -ne 0 ]; then _no "$1: not classified (rc=$_r)"; return; fi
    case "$_o" in *"$4"*) _ok "$1" ;; *) _no "$1: got [$_o]" ;; esac
  }
  _neg() { # <label> <rc> <output>
    _o=$(vm_fault_class "$2" "$3"); _r=$?
    if [ "$_r" -eq 0 ]; then _no "$1: MISCLASSIFIED as [$_o]"; else _ok "$1 (falls through)"; fi
  }

  echo "SELFTEST harness-vmfault.sh"

  # --- the five positives, transcribed from measured output -----------------
  _pos "bad --java-home" 1 \
"[cratonvm] main-vm run() returned Err: --java-home path does not exist or is not a directory: /c/Program Files/Microsoft/jdk-25.0.3.9-hotspot
Provide a valid JDK installation (must contain \`jmods/\` or \`lib/modules\`).
[cratonvm] jdk mode: <not yet resolved — the failure occurred before the class library was selected>" \
    "REJECTED THE JDK IMAGE"

  _pos "unknown flag" 2 \
"error: unexpected argument '--no-such-flag' found

  tip: to pass '--no-such-flag' as a value, use '-- --no-such-flag'

Usage: cratonvm-r10.exe --java-home <PATH> [CLASS_NAME] [ARGS]..." \
    "REJECTED ITS COMMAND LINE"

  _pos "flag missing a value" 2 \
"error: a value is required for '--java-home <PATH>' but none was supplied

For more information, try '--help'." \
    "REJECTED ITS COMMAND LINE"

  _pos "missing main class" 1 \
"[cratonvm] main-vm run() returned Err: Could not find or load main class NoSuchMain: class file error: class not found: NoSuchMain
[cratonvm] jdk mode: real-jdk (java.home=C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot)" \
    "MAIN CLASS NOT FOUND"

  # The non-image startup failure: same banner, no image complaint.
  _pos "startup failure that is not the image" 1 \
"[cratonvm] main-vm run() returned Err: could not open the class path entry /nope
[cratonvm] jdk mode: <not yet resolved — the failure occurred before the class library was selected>" \
    "BEFORE RESOLVING ITS JDK MODE"

  _pos "timeout"    124 "" "TIMED OUT"
  _pos "not +x"     126 "" "COULD NOT BE EXECUTED"
  _pos "no binary"  127 "" "COULD NOT BE EXECUTED"

  # --- the negative controls ------------------------------------------------
  # (a) a REAL assertion failure must fall through to run.sh's own sig grep.
  _neg "real AssertionError" 1 \
"CK RJdkFoo len=3
Exception in thread \"main\" java.lang.AssertionError: expected 4, got 3
	at RJdkFoo.main(RJdkFoo.java:41)
[cratonvm] jdk mode: real-jdk (java.home=C:/jdk)"

  # (b) a vector that legitimately exits 2 and prints its own `error:` line but
  #     no parser banner. Requiring all three signals is what saves this.
  _neg "vector exits 2 with its own error: line" 2 \
"CK RJdkBar step=1
error: the fixture decided to exit 2
[cratonvm] jdk mode: jdk-only (java.home=C:/jdk)"

  # (c) a vector whose OUTPUT quotes the parser banner as data — e.g. a fixture
  #     that asserts on a help string. `jdk mode: <not yet resolved` is checked
  #     as a whole-line banner the VM emits, so a quoted `error:` alone is inert.
  _neg "vector printing the word Usage" 1 \
"CK RJdkHelp text=Usage: cratonvm --java-home <PATH>
Exception in thread \"main\" java.lang.AssertionError: usage text changed
[cratonvm] jdk mode: real-jdk (java.home=C:/jdk)"

  # (d) a clean rc=0 run is never classified.
  _neg "clean pass" 0 \
"PASS RJdkFoo
[cratonvm] jdk mode: jdk-only (java.home=C:/jdk)"

  # (e) a VM crash the existing sig grep DOES handle must still fall through.
  _neg "SIGSEGV" 139 \
"[cratonvm] fatal runtime error: SIGSEGV
[cratonvm] jdk mode: real-jdk (java.home=C:/jdk)"

  # (f) every classification is ONE line. A four-line `why` would print once per
  #     vector, and the environment fault this exists for kills all 105 of them.
  for _rc_out in "1|[cratonvm] jdk mode: <not yet resolved — x" \
                 "2|error: unexpected argument
Usage: cratonvm" \
                 "1|Could not find or load main class Foo" \
                 "124|" "127|"; do
    _o=$(vm_fault_class "${_rc_out%%|*}" "${_rc_out#*|}")
    _n=$(printf '%s\n' "$_o" | grep -c .)
    [ "$_n" -eq 1 ] || { _no "one-line rule: [${_rc_out%%|*}] produced $_n lines"; }
  done
  _ok "every classification is exactly one line"

  # (g) vm_fault_hint covers every class the classifier can emit, and refuses
  #     anything else. A hint table that has silently fallen behind the
  #     classifier is the `H23` shape: green, and inert.
  for _cls in "REJECTED THE JDK IMAGE" "BEFORE RESOLVING ITS JDK MODE" \
              "REJECTED ITS COMMAND LINE" "MAIN CLASS NOT FOUND" "TIMED OUT" \
              "COULD NOT BE EXECUTED"; do
    if vm_fault_hint "rc=1: HARNESS FAULT — $_cls: x" > /dev/null; then :; else
      _no "vm_fault_hint has no entry for '$_cls'"; fi
  done
  if vm_fault_hint "rc=1: something else" > /dev/null; then
    _no "vm_fault_hint accepted a why it has no entry for"
  else
    _ok "vm_fault_hint covers all six classes and refuses anything else"
  fi

  # --- the fault this classifier is FOR, stated as a regression check -------
  # The current run.sh alternation must NOT match the trap-1 output. If a future
  # VM starts printing `fatal` on a bad --java-home this check goes red, which
  # is the right time to notice the two paths have merged.
  _sig=$(printf '%s\n' \
"[cratonvm] main-vm run() returned Err: --java-home path does not exist or is not a directory: /c/x
[cratonvm] jdk mode: <not yet resolved — the failure occurred before the class library was selected>" \
    | grep -aiE 'AssertionError|NoSuchMethod|linkage error|panic|SEGV|fatal' | grep -avE '^\s*at ')
  if [ -z "$_sig" ]; then
    _ok "run.sh's existing sig grep is still blind to the trap-1 output (the premise)"
  else
    _no "premise broken: the existing sig grep now matches [$_sig] — re-read this file"
  fi

  [ "$_f" -eq 0 ] && { echo "  selftest OK — 8 faults classified, 5 negative controls fall through,
  the one-line rule holds, the hint table is complete, and the premise holds"; exit 0; }
  exit 3
fi
