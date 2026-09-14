#!/usr/bin/env bash
# The regression suite's INSTRUMENT CHECK.
#
# WHY THIS FILE EXISTS
# --------------------
# run.sh reduces every vector's output to its PASS/CK lines before diffing the
# two VMs (see extract() below). That filter is right — CratonVM interleaves
# timestamped WARN/tracing noise with the vector's output and a raw diff would
# be red on every class for reasons that have nothing to do with the VM — but
# for a long time NOTHING CHECKED THAT ANYTHING MEANINGFUL SURVIVED IT.
#
# Three scheduled vectors printed their entire evidence on other prefixes.
# extract() reduced each of them to the constant `PASS <Class>`, and a constant
# string always matches itself. Measured side by side in
# W7-51-vacuous-sweep-round-2.md: RDataInputFastPull with a one-line defect
# injected — a typed read dropping the high byte of readShort(), exactly the
# partial fast pull the vector exists to catch — exited rc=0 with output
# BYTE-IDENTICAL to a clean run. The vector was scheduled, it ran, and it could
# not fail.
#
# Repairing those three vectors fixes three vectors. It does not stop the
# fourth. This file is the part that does: it makes the harness state, per
# vector and on every run, that the comparison it is about to report on is a
# comparison it can actually lose.
#
# THE REPORTING DIALECT — the contract, written down
# --------------------------------------------------
# Seven fixtures have now been found reporting in a spelling this harness either
# DELETES or CANNOT PARSE (RShutdownHooks, RSimpleTimeZoneRaw,
# RJdkStringCodePoints, RFsSingleton, RArrayStoreTiers, RArrayStoreInterfaces,
# RSslNullSession). That is not seven careless authors — it is a contract that
# lived in a `grep` expression and in three records, and nowhere a fixture author
# would look. So it is stated here, and G6 below enforces it:
#
#   EVIDENCE   CK <Class> <key>=<value>        one observable per line, and the
#                                              value is the VM's OWN answer, not
#                                              the fixture's expectation. The two
#                                              VMs must diff their answers
#                                              against EACH OTHER; a fixture that
#                                              prints only its own verdict has
#                                              made the comparison unlosable.
#   COUNT      CK <Class> checks=N             the WHOLE rest of the line, and
#                                              nothing else on it.
#   FAILURES   CK <Class> fails=N              a SEPARATE line from the count.
#   BANNER     PASS <Class> (N checks)         parenthesised, clean path only.
#
# Everything else on stdout/stderr is deleted before the diff. In particular:
#
#   NOT `RESULT <Class> PASS`         — run.sh greps ^PASS <Class>; inverted.
#   NOT `PASS <Class> checks=N`       — the PASS arm of harness_check_count
#                                       REQUIRES the parentheses; the count reads
#                                       as absent.
#   NOT `CK <Class> checks=N fails=M` — harness_check_count takes the whole rest
#                                       of the line, so the "count" is the STRING
#                                       "N fails=M"; G3's `-eq 0` is then a
#                                       syntax error swallowed by its own
#                                       2>/dev/null and the guard SILENTLY
#                                       NO-OPS. The most dangerous of the set.
#   NOT `ok <label>=<value>`, `@@RESULT …`, `DIVERGENCE …`, lowercase `pass`.
#
# The recorded judgement, restated so it is not re-litigated by accident:
# WIDENING THE PARSER TO ACCEPT THESE VARIANTS IS THE WRONG DIRECTION
# (W8-E9-1 NOM-4). 76 of 77 counting vectors already use the parenthesised
# spelling; a parser that accepts both makes a slip stop being visible, which is
# the same species of defect as a guard that cannot fire. G6 goes the other way
# and makes the near-miss LOUD.
#
# THE SIX GUARDS
# --------------
#   G1 DISCARDED EVIDENCE   the oracle printed a line extract() deletes.
#   G2 CONSTANT EXTRACT     what survives extract() carries no observable at
#                           all, so the cross-VM diff compares a constant
#                           against itself.
#   G3 NO CHECK COUNT       the vector publishes no count of the assertions it
#                           executed, so a run that silently asserted FEWER
#                           things than the oracle is indistinguishable from a
#                           run that asserted all of them. Ratcheted, not
#                           fatal-on-sight — see harness-uncounted.txt.
#   G4 SICK ORACLE          the HotSpot run that supplies ground truth did not
#                           itself succeed, so the "expected" side of the diff
#                           is a truncated artefact of the oracle's failure.
#   G5 MISLABELLED REACH    a vector is scheduled into a mode where the code a
#                           NAMED FAMILY of its rows targets does not run, so
#                           that family passes for reasons unrelated to what it
#                           was written to catch. Ratcheted in both directions
#                           against the table in run.sh — see
#                           harness_guard_nondiscriminating.
#   G6 DIALECT NEAR-MISS    the vector reports in a spelling one character away
#                           from the contract above. Two arms, because the two
#                           halves are visible to nothing else: on a DELETED
#                           prefix G6 upgrades G1's generic "you dropped a line"
#                           into a named diagnosis, and on a KEPT prefix — where
#                           G1 is blind by construction — it is the only thing
#                           that fires at all. Not a fifth population: it reports
#                           through G1's and G3's own return values.
#
# G1-G4 all reason about a vector's OUTPUT. G5 is the one that cannot: whether a
# family reaches the implementation it names is a fact about which Rust natives
# answer in which runtime mode, and no amount of reading the vector's stdout
# recovers it. That is why it is a hand-maintained table with a ratchet rather
# than a computation, and why its unit is the FAMILY and not the vector — the
# case it was built for (RJdkOptionalShape's httpmint block) sits inside a
# fixture whose six OTHER families are real, load-bearing default-mode coverage.
# A guard that could only say "this vector is vacuous" would have to be wrong
# about that one, and would then be argued with instead of fixed.
#
# A guard that has never been shown to fire is the same species of defect as
# the one it is guarding against. All four were mutation-checked; the evidence
# is in docs/known-issues/jdk-only/W7-60-harness-extract-blindness.md and the
# mutants are reproducible with harness-selfcheck.sh, which runs the guards
# against HotSpot alone and therefore needs no CratonVM build.
#
# Sourced by run.sh and by harness-selfcheck.sh. Defines exactly one copy of
# extract(), so the filter the suite diffs through and the filter the guards
# reason about cannot drift apart.

# Extract only the deterministic test lines (PASS/CK), stripping CratonVM's
# timestamped WARN/tracing noise and ANSI colour, so the cross-VM diff is clean.
extract() { sed 's/\x1b\[[0-9;]*m//g' | grep -aE '^(PASS|CK) ' ; }

# Lines a JDK writes to its own stderr that are NOT vector evidence, and so are
# legitimately dropped. Deliberately a MEASURED list, not a defensive one: the
# only shape that occurs across the 70 scheduled vectors on Temurin 25.0.3+9 is
# the restricted-method WARNING block from System.loadLibrary (RJdkJni,
# RJdkFailure, four lines each). Widening this pattern is how G1 would be
# talked out of firing, so widen it only against a measurement.
HARNESS_NOISE_RE='^(WARNING: |Picked up (JAVA_TOOL_OPTIONS|_JAVA_OPTIONS)|OpenJDK [0-9A-Za-z_-]+ (Server )?VM warning:)'

# ---------------------------------------------------------------------------
# harness_dialect_nearmiss <class>   (stdin = candidate lines)
#
# G6, deleted-prefix half. Classifies a line extract() threw away against the
# spellings that have ACTUALLY occurred in this tree — a measured list, like
# HARNESS_NOISE_RE, not a defensive one. Every arm below names a fixture:
#
#   RESULT <Class> PASS      RSimpleTimeZoneRaw   (W8-E9-1 §2)
#   @@RESULT checks=N …      RJdkStringCodePoints, RFsSingleton (§1, W8-E15-1 §1)
#   ok <label>=<value>       RJdkStringCodePoints, RFsSingleton
#   <key>=<value>, bare      RArrayStoreTiers, RArrayStoreInterfaces (W8-E15-1
#                            NOM-2) — 17 and 29 lines of per-row cold=/hot= tier
#                            evidence, on no prefix at all
#   DIVERGENCE <detail>      both array-store fixtures, on the RED path
#
# Adds NO point of its own: it prints inside G1's message, which has already
# counted the line. The value is the remedy, spelled out, next to the offending
# text — a guard that says "this line was deleted" and a guard that says "write
# it as CK <Class> <key>=<value> instead" get fixed at very different rates.
# ---------------------------------------------------------------------------
harness_dialect_nearmiss() {
  awk -v cls="$1" '
    {
      l = $0; sub(/\r$/, "", l); d = "";
      if      (l ~ ("^RESULT " cls "([^A-Za-z0-9_]|$)"))  d = "banner INVERSION — run.sh and G4 grep ^PASS " cls ". Write: PASS " cls " (N checks)";
      else if (l ~ ("^" cls " +(PASS|FAIL|OK|ok)"))       d = "banner INVERSION — write: PASS " cls " (N checks)";
      else if (l ~ /^@@RESULT/)                           d = "@@RESULT is a DELETED prefix — write the count as: CK " cls " checks=N (and fails on its own line)";
      else if (l ~ /^(ok|OK|Ok) +[^ ]+ *=/)               d = "the `ok <label>=<value>` dialect — write: CK " cls " <label>=<value>";
      else if (l ~ /^(pass|Pass|ck|Ck|cK) /)              d = "CASE — the filter is case-SENSITIVE. Write PASS / CK in capitals";
      else if (l ~ /^(DIVERGENCE|FAILED|FAIL|MISMATCH) /) d = "failure detail on a deleted prefix — on a red run the harness sees THAT something failed and not WHAT. Write: CK " cls " FAILED <detail>";
      else if (l ~ /checks?[=:] *[0-9]/)                  d = "publishes a COUNT the harness cannot read — write: CK " cls " checks=N";
      else if (l ~ /=/)                                   d = "carries a `key=value` observable on a deleted prefix — write: CK " cls " <key>=<value>";
      if (d != "") printf "        NEAR-MISS: %s\n          -> %s\n", l, d;
    }
  '
}

# Does this extracted block publish how many assertions ran? Two accepted
# spellings, both of which survive extract():
#     PASS RFoo (42 checks)
#     CK RFoo checks=42
# Zero is not a count: a vector that reports `checks=0` executed no assertion.
harness_check_count() {
  # $1 = class, stdin = extracted output. Echoes the count, or nothing.
  awk -v cls="$1" '
    $0 ~ "^CK " cls " checks=" { sub(/^.*checks=/, ""); print; found=1; exit }
    $0 ~ "^PASS " cls "( |$)" {
      if (match($0, /\(([0-9]+) checks?\)/)) {
        s = substr($0, RSTART + 1, RLENGTH - 2); sub(/ checks?$/, "", s); print s; found=1; exit
      }
    }
  '
}

# Read harness-uncounted.txt into $HARNESS_UNCOUNTED (space-delimited, with
# leading and trailing spaces so `case` membership tests are exact).
harness_load_uncounted() {
  HARNESS_UNCOUNTED=" "
  [ -f "$1" ] || return 0
  while IFS= read -r line; do
    line=${line%%#*}
    for w in $line; do HARNESS_UNCOUNTED="$HARNESS_UNCOUNTED$w "; done
  done < "$1"
}

# ---------------------------------------------------------------------------
# harness_guard_extract <class> <extracted-file>
#
# G2 and G3. Needs only what survived the filter, so it runs on every invocation
# — including on a host with no HotSpot, where the cross-VM diff is skipped for
# every class and these are the ONLY thing standing between "the vector printed
# its banner" and "the vector measured something".
#
# Appends one line per violation to $HARNESS_GUARD_MSGS and returns 1 if any.
# ---------------------------------------------------------------------------
harness_guard_extract() {
  gc_class="$1"; gc_key="$2"; gc_bad=0
  # `grep -c` prints its count AND exits 1 when the count is zero, so a
  # `|| echo 0` fallback would append a SECOND number and every arithmetic test
  # below would break on "0\n0". Take the count and default the empty case.
  gc_lines=$(grep -ac . "$gc_key" 2>/dev/null); gc_lines=${gc_lines:-0}
  # Any `CK ` line counts as an observable. Deliberately NOT anchored on the
  # class name: several vectors label their CK lines by the SUBJECT rather than
  # the class (`CK tm-range 1234`, `CK pbq-sorted 5678`), and those carry a real
  # measurement. Only the check-count parse below is class-anchored, because
  # that one has to read a number out of a specific line.
  gc_ck=$(grep -ac '^CK ' "$gc_key" 2>/dev/null); gc_ck=${gc_ck:-0}
  gc_count=$(harness_check_count "$gc_class" < "$gc_key")

  # G2. An observable is a CK line or a published check count. Without one of
  # the two, everything that reaches the diff is fixed text that the vector
  # would print whatever the VM did with it.
  if [ "$gc_ck" -eq 0 ] && [ -z "$gc_count" ]; then
    if [ "$gc_lines" -eq 0 ]; then
      HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G2] $gc_class: nothing survives extract() — the cross-VM diff compares two empty strings"
    else
      HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G2] $gc_class: extract() leaves a CONSTANT ($gc_lines line(s), no CK line, no check count).
    A constant always matches itself, so this vector cannot fail the diff. Print
    its observables on 'CK $gc_class <key>=<value>' lines, or publish a check count."
    fi
    gc_bad=1
  fi

  # G3. Ratcheted against harness-uncounted.txt in BOTH directions: a vector
  # that stops publishing a count is a regression, and a vector that starts
  # publishing one is not repaired until its row is deleted. A baseline that
  # only records "known bad" decays into a list nobody re-checks.
  case "$HARNESS_UNCOUNTED" in
    *" $gc_class "*) gc_listed=1 ;;
    *) gc_listed=0 ;;
  esac
  if [ -z "$gc_count" ] || [ "$gc_count" -eq 0 ] 2>/dev/null; then
    if [ "$gc_listed" -eq 0 ]; then
      HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G3] $gc_class: publishes no check count, and is not in regression-suite/harness-uncounted.txt.
    Without a count, a run that silently asserted FEWER things than the oracle
    diffs identically. Emit 'PASS $gc_class (N checks)' or 'CK $gc_class checks=N'."
      gc_bad=1
    fi
  elif [ "$gc_listed" -eq 1 ]; then
    HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G3] $gc_class: publishes a check count ($gc_count) but is still listed in
    regression-suite/harness-uncounted.txt. Delete its row — the ratchet only holds
    if clearing an entry is what makes the run green again."
    gc_bad=1
  fi

  # ---- G6, the KEPT-PREFIX half -------------------------------------------
  #
  # These two spellings survive extract(), so G1 — which reasons about what was
  # DELETED — is blind to them by construction, and G3 either misreports them as
  # "no count" or silently no-ops. They are the reason G6 exists as a guard and
  # not just as a better G1 message.
  #
  # Neither is hypothetical and neither is common — which is exactly the profile
  # that makes a lint worth having and a widened parser not. MEASURED over the 94
  # scheduled vectors: N1 one (RShutdownHooks, W8-E9-1 §3), N2 two
  # (RSslNullSession and RJdkProcess), N3 none, and 76+ on the correct spelling.
  # RJdkProcess was found BY THIS GUARD on its first run over the corpus, after
  # four hand-censuses had missed it — see W8-E30-1 §4.4.

  # N1. `PASS <Class> checks=4` — RShutdownHooks, and it hid a published count
  # for a year (W8-E9-1 §3). harness_check_count's PASS arm requires the
  # parentheses, so the count reads as ABSENT and the remedy looks like "add it
  # to harness-uncounted.txt" rather than "add two characters".
  gc_np=$(grep -aE "^PASS $gc_class([^A-Za-z0-9_]|\$)" "$gc_key" 2>/dev/null \
          | grep -a 'checks=' | grep -avE '\([0-9]+ checks?\)' | head -1)
  if [ -n "$gc_np" ]; then
    HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G6] $gc_class: the PASS banner publishes its count in the UNPARENTHESISED
    spelling, which harness_check_count's PASS arm does not accept:
      | $gc_np
    Write 'PASS $gc_class (N checks)'. The parser is deliberately NOT widened to
    accept both — 76 of 77 counting vectors use the parenthesised form, and a
    parser that takes either makes this slip stop being visible."
    gc_bad=1
  fi

  # N2. `CK <Class> checks=47 failures=0` — RSslNullSession. The WORST of the
  # near-misses, because it fails silently in the guard rather than in the
  # vector: harness_check_count does `sub(/^.*checks=/, ""); print`, so it
  # returns the STRING "47 failures=0"; G3 then evaluates
  # `[ "47 failures=0" -eq 0 ]`, which is a syntax error swallowed by its own
  # 2>/dev/null, and G3 neither passes nor fires. Anything after the number on
  # that line trips this, which is the right predicate: the parser's contract is
  # that the count is the whole rest of the line.
  gc_cc=$(grep -aE "^CK $gc_class checks=" "$gc_key" 2>/dev/null \
          | grep -avE "^CK $gc_class checks=[0-9]+[[:space:]]*\$" | head -1)
  if [ -n "$gc_cc" ]; then
    HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G6] $gc_class: the count line carries more than the count, so
    harness_check_count returns a non-numeric string and G3 SILENTLY NO-OPS:
      | $gc_cc
    One value per line — 'CK $gc_class checks=N', and put fails/failures on a
    line of its own."
    gc_bad=1
  fi

  # N3. Two published counts that disagree. Cheap, and the only thing that can
  # notice a banner whose literal drifted from the counter that feeds it.
  gc_pc=$(grep -aE "^PASS $gc_class([^A-Za-z0-9_]|\$)" "$gc_key" 2>/dev/null \
          | sed -n 's/.*(\([0-9][0-9]*\) checks*).*/\1/p' | head -1)
  gc_kc=$(grep -aE "^CK $gc_class checks=[0-9]+[[:space:]]*\$" "$gc_key" 2>/dev/null \
          | sed -n 's/^.*checks=\([0-9][0-9]*\).*/\1/p' | head -1)
  if [ -n "$gc_pc" ] && [ -n "$gc_kc" ] && [ "$gc_pc" != "$gc_kc" ]; then
    HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G6] $gc_class: publishes TWO check counts and they disagree —
    'CK $gc_class checks=$gc_kc' vs 'PASS $gc_class ($gc_pc checks)'. One of them is a
    literal that stopped tracking its counter."
    gc_bad=1
  fi
  return $gc_bad
}

# ---------------------------------------------------------------------------
# harness_guard_oracle <class> <oracle-raw-file> <oracle-rc>
#
# G1 and G4. Both need the ORACLE's raw output specifically, and the reason is
# the whole trick: HotSpot prints exactly what the vector prints and nothing
# else, so the set of lines extract() drops from IT is precisely the evidence
# the harness is blind to. The same subtraction against CratonVM's output would
# be swamped by the VM's own tracing and could never be made fatal.
# ---------------------------------------------------------------------------
harness_guard_oracle() {
  go_class="$1"; go_raw="$2"; go_rc="$3"; go_bad=0

  # G4 first: everything G1 says about a sick oracle's output is uninteresting.
  if [ "$go_rc" -ne 0 ]; then
    go_why="rc=$go_rc"
    [ "$go_rc" -eq 124 ] && go_why="rc=124 (TIMED OUT — its output is a truncated prefix)"
    HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G4] $go_class: the HotSpot oracle run FAILED ($go_why), so the
    'expected' side of the cross-VM diff is an artefact of the oracle's failure,
    not ground truth. First dropped line: $(grep -avE "$HARNESS_NOISE_RE" "$go_raw" | grep -a . | head -1 | head -c 100)"
    return 1
  fi
  if ! grep -qaE "^PASS $go_class([^A-Za-z0-9_]|\$)" "$go_raw"; then
    # G6 on the way past. G4 RETURNS EARLY, so on RSimpleTimeZoneRaw — which
    # printed `RESULT RSimpleTimeZoneRaw PASS` — the G1 that the same line would
    # have tripped never appeared in the log, and fixing only the missing banner
    # would have swapped one guard for another (W8-E9-1 §2). Say both things at
    # once, here, where the reader is.
    go_nm=$(grep -a . "$go_raw" | grep -avE '^(PASS|CK) ' | grep -avE "$HARNESS_NOISE_RE" \
            | harness_dialect_nearmiss "$go_class" | head -8)
    HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G4] $go_class: the HotSpot oracle exited 0 but printed no 'PASS $go_class' line.${go_nm:+
    A banner in a near-miss spelling IS present, so this is a dialect defect and not a
    silent vector — and fixing only the banner would leave the G1 below:
$go_nm}"
    return 1
  fi

  # G1. This is the defect the file is named for, stated as a predicate.
  go_dropped=$(grep -a . "$go_raw" | grep -avE '^(PASS|CK) ' | grep -avE "$HARNESS_NOISE_RE")
  if [ -n "$go_dropped" ]; then
    go_n=$(printf '%s\n' "$go_dropped" | grep -c .)
    # G6, deleted-prefix half: name the spelling rather than only the loss.
    go_nm=$(printf '%s\n' "$go_dropped" | harness_dialect_nearmiss "$go_class" | head -12)
    HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G1] $go_class: the oracle printed $go_n line(s) that extract() DELETES before the diff.
    Evidence on a non-PASS/CK prefix is evidence the suite cannot see. Move it to
    'CK $go_class <key>=<value>'. Dropped:
$(printf '%s\n' "$go_dropped" | head -6 | sed 's/^/      | /')${go_nm:+
    [G6] recognised spellings among them:
$go_nm}"
    go_bad=1
  fi
  return $go_bad
}

# The runtime-mode token G5's ADDITION scan searches vector sources for. Exactly
# one token, and the scope is argued rather than assumed:
#
#   `--synthetic-jdk` needs a binary built with `--features synthetic-jdk`; a
#   stock build refuses the flag and exits 1. Nothing this suite can do supplies
#   it, so a family whose target only answers there is STRUCTURALLY incapable of
#   failing in any run this harness performs. That is G5's subject.
#
#   `--jdk-only`, by contrast, is a flag the harness CAN supply and does supply
#   per class (class_cv_args's RJdkSqlPackage arm). A family needing it is
#   MISCONFIGURED, not inert, and the fix is a line in class_cv_args — so
#   folding it in here would turn a fixable registration into a permanent table
#   entry, and would put most of the RJdk* corpus in a table meant to stay small
#   enough to re-read.
#
# The pattern is the bare feature/mode name so it catches both the flag spelling
# (`--synthetic-jdk`) and the Cargo-feature spelling (`the synthetic-jdk Cargo
# feature`), which are the two ways the sources actually write it.
HARNESS_MODE_RE='synthetic-jdk'

# ---------------------------------------------------------------------------
# harness_guard_nondiscriminating <src-dir> <listed-classes>
#
# G5, read out of $HARNESS_NONDISCRIMINATING (defined in run.sh, one row per
# line, `#` comments allowed):
#
#     <Class>|<INERT|LIVE>|<family>|<mode>|<reason>
#
# INERT  one NAMED FAMILY of this vector's rows targets code that only runs in
#        <mode>, which is not the mode the vector is scheduled in. The family
#        still executes and still passes; it just cannot fail for the reason it
#        was written. Everything else in the vector is untouched by this row.
# LIVE   this vector's source NAMES <mode>, so the addition scan below would
#        otherwise flag it — and it has been adjudicated as genuinely
#        discriminating in the mode it is scheduled in. The row is the record
#        of that adjudication, not a defect.
#
# TWO-WAY RATCHET, on harness-uncounted.txt's model — the direction that decays
# is the one that only ever records "known bad", so both are loud here:
#
#   ADD    a listed vector whose source names <mode> and has NO row. Adding a
#          mode-straddling fixture forces an explicit INERT/LIVE verdict.
#   STALE  a row whose class no longer exists, is no longer in any class list,
#          whose INERT family is no longer named in the source, whose LIVE
#          source no longer mentions the mode, or — the repair signal — whose
#          run IS executing <mode>, at which point the family is live and the
#          row must go.
#
# WHAT IT CANNOT DO, stated so nobody quotes it for more than it measures: a
# family that is inert and whose source says nothing about the mode is NOT
# detectable here, and the motivating row is exactly that case (it was found by
# measuring which Rust file answers, not by reading the fixture). The ADD scan
# catches the syntactically visible half; the table carries the rest by hand.
# This is the same residual blindness G3 has, and it is the reason G5 is a
# ratchet rather than a census.
#
# Appends to $HARNESS_GUARD_MSGS; sets $HARNESS_G5_BAD (one point per offending
# class) and $HARNESS_G5_CLASSES. Returns 1 if any fired.
# ---------------------------------------------------------------------------
harness_guard_nondiscriminating() {
  gn_src="$1"; gn_listed="$2"
  HARNESS_G5_BAD=0; HARNESS_G5_CLASSES=""; gn_named=" "
  # Read from a HERE-DOC, never a pipe: `... | while read` runs the loop in a
  # SUBSHELL, so every counter below would be discarded and the guard would
  # silently never fire — the same shape run.sh's prune_missing comment warns
  # about.
  while IFS='|' read -r gn_cls gn_verdict gn_fam gn_mode gn_why; do
    case "$gn_cls" in ''|'#'*|' '*'#'*) continue ;; esac
    gn_cls=$(printf '%s' "$gn_cls" | tr -d ' \t')
    [ -n "$gn_cls" ] || continue
    gn_named="$gn_named$gn_cls "
    gn_msg=""
    case "$gn_verdict" in
      INERT|LIVE) ;;
      *) gn_msg="malformed row: verdict '$gn_verdict' is not INERT or LIVE" ;;
    esac
    if [ -z "$gn_msg" ] && [ ! -f "$gn_src/$gn_cls.java" ]; then
      gn_msg="STALE row: src/$gn_cls.java does not exist. Delete the row."
    fi
    if [ -z "$gn_msg" ]; then
      case " $gn_listed " in
        *" $gn_cls "*) ;;
        *) gn_msg="STALE row: '$gn_cls' is in no class list, so there is no schedule to mislabel. Delete the row." ;;
      esac
    fi
    if [ -z "$gn_msg" ] && [ "$gn_verdict" = INERT ]; then
      if ! grep -qa -- "$gn_fam" "$gn_src/$gn_cls.java" 2>/dev/null; then
        gn_msg="STALE row: family '$gn_fam' is no longer named in src/$gn_cls.java. Re-adjudicate or delete the row."
      elif [ -n "$gn_mode" ]; then
        case " ${CRATONVM_ARGS:-} " in
          *" $gn_mode "*)
            gn_msg="STALE row: this run executes $gn_mode, so '$gn_fam' IS discriminating here. The row records the opposite; delete it or scope it."
            ;;
        esac
      fi
    fi
    if [ -z "$gn_msg" ] && [ "$gn_verdict" = LIVE ]; then
      if ! grep -qaE "$HARNESS_MODE_RE" "$gn_src/$gn_cls.java" 2>/dev/null; then
        gn_msg="STALE row: src/$gn_cls.java no longer names a non-default runtime mode, so this LIVE adjudication has nothing left to adjudicate. Delete the row."
      fi
    fi
    if [ -n "$gn_msg" ]; then
      HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G5] $gn_cls: $gn_msg
    The table is HARNESS_NONDISCRIMINATING in regression-suite/run.sh."
      HARNESS_G5_BAD=$((HARNESS_G5_BAD+1)); HARNESS_G5_CLASSES="$HARNESS_G5_CLASSES $gn_cls"
    fi
  done <<HARNESS_G5_ROWS
$HARNESS_NONDISCRIMINATING
HARNESS_G5_ROWS

  # ADD direction. Scoped to LISTED classes rather than to the classes this
  # particular run scheduled, so G5's verdict is the same under SUITE=core,
  # SUITE=jdk-only and SUITE=all — a ratchet whose answer depends on the
  # invocation is one every lane learns to attribute to the invocation.
  for gn_f in "$gn_src"/*.java; do
    [ -f "$gn_f" ] || continue
    gn_b=$(basename "$gn_f" .java)
    case " $gn_listed " in *" $gn_b "*) ;; *) continue ;; esac
    grep -qaE "$HARNESS_MODE_RE" "$gn_f" || continue
    case "$gn_named" in
      *" $gn_b "*) continue ;;
    esac
    HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [G5] $gn_b: scheduled, and its source names a runtime mode ($HARNESS_MODE_RE)
    that no run of this suite can execute — a stock binary refuses --synthetic-jdk. Say which:
    add a row to HARNESS_NONDISCRIMINATING in regression-suite/run.sh reading
    '$gn_b|INERT|<family>|--synthetic-jdk|<why>' if some named family of its rows targets code
    that only answers there, or '$gn_b|LIVE|-|--synthetic-jdk|<why>' if its rows discriminate
    against whatever implementation answers in the mode it is scheduled in."
    HARNESS_G5_BAD=$((HARNESS_G5_BAD+1)); HARNESS_G5_CLASSES="$HARNESS_G5_CLASSES $gn_b"
  done
  [ "$HARNESS_G5_BAD" -eq 0 ]
}

# ===========================================================================
# PER-CLASS LAUNCH HOOKS — the single definition
# ===========================================================================
#
# run.sh and harness-selfcheck.sh both schedule the SAME class lists (the
# selfcheck reads them out of run.sh precisely so they cannot drift), and both
# have to launch each vector with whatever that vector needs. They had two
# independent answers to that: run.sh grew class_args()/class_cp_extra() for
# RServiceLoaderDoubleSource on 2026-08-13, the selfcheck still had one
# hard-coded `[ "$c" = RJdkModule ]` line, and the newly-scheduled vector was
# therefore flagged G4+G3 in the selfcheck while being correctly wired in the
# suite. A guard that reports a defect the tree does not have is on its way to
# being ignored.
#
# So the hooks live here, next to extract(), for exactly the reason stated at
# the top of this file for extract(): the definition run.sh launches with and
# the definition the guards launch with drifting apart is the same defect one
# level up.
#
# MIGRATION NOTE — run.sh still carries its own copies, and this file is not a
# no-op meanwhile. run.sh sources this file BEFORE it defines them, so its
# copies shadow these and its behaviour is bit-for-bit unchanged; the selfcheck
# does not define them at all and so uses these. harness_hooks_drift() below
# holds the two behaviourally identical until run.sh's copies are deleted, at
# which point it goes quiet on its own. See the nominations in
# docs/known-issues/jdk-only/W8-E30-1-*.md.
#
# Callers must set: HAVE_MODULE, MODBUILD, JDKONLY_MODULE (and, for
# class_cv_args, CRATONVM_ARGS). CPSEP is defaulted below if the caller has none.

# The CLASS-PATH separator, for the one vector that needs a second entry
# (class_cp_extra). Load-bearing and platform-dependent: `;` on Windows, `:`
# everywhere else. Derived from `uname` rather than from $JDK, because the suite
# is run from Git Bash on Windows (where java.exe wants `;`) and from a POSIX
# shell on the Linux build host (where it wants `:`), and $JDK looks the same in
# both. `:=` so a caller that already computed it — run.sh does, before sourcing
# this file — keeps its own value and the two cannot disagree.
if [ -z "${CPSEP:-}" ]; then
  case "$(uname -s 2>/dev/null)" in
    MINGW*|MSYS*|CYGWIN*|*NT-*) CPSEP=';' ;;
    *)                          CPSEP=':' ;;
  esac
fi
: "${JDKONLY_MODULE:=cratonvm.jdkonly.svc}"

# Launcher arguments a specific vector needs, in a spelling BOTH VMs accept —
# these are handed to HotSpot too, so the oracle runs the same shape. Emitted
# as a word list, consumed unquoted.
class_args() {
  case "$1" in
    RJdkModule)
      [ -n "$HAVE_MODULE" ] && printf '%s' "--module-path $MODBUILD --add-modules $JDKONLY_MODULE"
      ;;
    # The three properties that name the modular class-path entry
    # class_cp_extra() supplies. Handed to BOTH VMs (CratonVM's launcher parses
    # -D into system properties the same way; vm-cli/src/main.rs), because the
    # oracle has to see the identical configuration or the diff is meaningless.
    #
    # This arm must NEVER grow a --module-path: the vector asserts
    # `System.getProperty("jdk.module.path") == null` as a PRECONDITION, since a
    # --module-path module IS resolved into the boot layer and IS defined to the
    # application loader on a real JVM — supplying both would make the vector
    # measure its own command line instead of the VM.
    RServiceLoaderDoubleSource)
      [ -n "$HAVE_MODULE" ] && printf '%s' \
        "-Dcratonvm.rt.cpmodule=$JDKONLY_MODULE -Dcratonvm.rt.cpclass=com.cratonvm.jdkonly.svc.Greeter -Dcratonvm.rt.cpservice=com.cratonvm.jdkonly.svc.Greeter"
      ;;
    *) : ;;
  esac
}

# Extra CLASS-PATH entries a vector needs, appended to the build directory,
# separator included so an empty answer is a literal no-op. Handed to BOTH VMs.
#
# RServiceLoaderDoubleSource needs a MODULAR artefact (here: the exploded module
# regression-suite/modules/ already compiles for RJdkModule) on the CLASS path,
# and it must be there WHEN THE VM STARTS — CratonVM scans the application class
# path for module-info.class inside ClassManager::new, before main, so a jar the
# vector built itself would be scanned by nobody. That is the whole reason this
# hook exists and the reason the vector could not simply be added to a list.
#
# The same directory must NOT also reach class_args() as a --module-path: see
# the note there.
#
# If the module could not be built, $HAVE_MODULE is empty and this emits
# nothing — the vector then fails on its own first assertion. That is deliberate
# and is the vector's own design: a precondition that silently disarms the only
# discriminating check is how a gate becomes green-forever.
class_cp_extra() {
  case "$1" in
    RServiceLoaderDoubleSource)
      [ -n "$HAVE_MODULE" ] && printf '%s' "$CPSEP$MODBUILD/$JDKONLY_MODULE"
      ;;
    *) : ;;
  esac
}

# CratonVM-ONLY arguments for a vector: launcher flags in CratonVM's own
# spelling, which HotSpot would reject outright. Kept separate from class_args
# for exactly that reason — a `--nojit` in class_args would make the oracle exit
# non-zero, its key lines come back empty, and every such vector would fail the
# cross-VM diff for a reason that has nothing to do with the VM.
#
# NOTHING IN harness-selfcheck.sh CALLS THIS: that script runs HotSpot alone.
# It lives here anyway so the three hooks are one family in one place; splitting
# them by "who happens to call it today" is how the next hook gets added to only
# one of two files.
#
# BOTH GC ENTRIES ARE INERT WITHOUT THEIR ARGUMENT — they do not merely lose
# sensitivity, they PASS ON A BROKEN VM, which is worse than not running at all
# because it reads as coverage: on the default heap no collection happens during
# the walk so the stale ObjectRef is never created, and with a live JIT frame on
# the stack the young generation falls back to a non-moving sweep under which a
# stale reference still resolves. Hence --Xmx 64m for both, and --nojit for
# RPriorityQueueGc only — RTreeRangeGc reproduces with the JIT on (3/3,
# b2e13e441), so withholding the flag is what keeps the default compiling
# configuration under test. Do not "make the two entries consistent".
class_cv_args() {
  case "$1" in
    RPriorityQueueGc) printf '%s' "--nojit --Xmx 64m" ;;
    RTreeRangeGc)     printf '%s' "--Xmx 64m" ;;
    # RClassUnloadSweepGen is RClassUnloadSweep re-run on the generational
    # collector. Without the flag it runs on the default (ZGC since
    # 2026-08-10) and is a byte-for-byte re-run of its twin -- a scheduled
    # vector that cannot fail for its own reason. It does not redden the
    # suite, which is precisely why the omission survives unnoticed.
    RClassUnloadSweepGen) printf '%s' "-XX:+UseGenerationalGC" ;;
    # CratonVM's Panama gate is default-CLOSED (NATIVE_ACCESS_POLICY in
    # native-builtins/src/panama.rs; only --enable-native-access opens it).
    # Without this every downcall in RJdkForeign raises IllegalCallerException
    # and the vector reddens for a reason that is not the thing it tests.
    # HotSpot needs no counterpart: on JDK 25 the restricted-method call only
    # WARNS, and the warning goes to stderr, which the cross-VM PASS/CK diff
    # does not read. `--enable-native-access` is declared require_equals, so
    # the `=` spelling is the safe one.
    RJdkForeign)      printf '%s' "--enable-native-access=ALL-UNNAMED" ;;
    # RJdkSqlPackage is a --jdk-only POLICY vector and is RED without the flag:
    # its nonVolatileRejected() arm asserts HotSpot's IllegalArgumentException
    # ("Must be volatile type"), which the Compatible-mode native in
    # native-builtins/src/atomic_updater.rs does not implement. MEASURED:
    # repeating the flag is NOT harmless — `cratonvm --jdk-only --jdk-only`
    # exits 2 ("cannot be used multiple times"), so emit it only when
    # CRATONVM_ARGS has not already supplied it.
    RJdkSqlPackage)
      case " ${CRATONVM_ARGS:-} " in
        *" --jdk-only "*) : ;;
        *) printf '%s' "--jdk-only" ;;
      esac
      ;;
    *) : ;;
  esac
}

# Emit every hook's answer for every class under a small ENVIRONMENT MATRIX, so
# two definitions can be compared by behaviour rather than by text. The matrix
# is not decoration: class_args and class_cp_extra branch on $HAVE_MODULE and
# class_cv_args branches on $CRATONVM_ARGS, so a comparison at one point in that
# space would pass over a difference in the other arms.
#
# No command substitution anywhere in the loop — the hooks printf to stdout and
# this function inherits it. `$(...)` per class would be ~1,100 forks on a
# 94-class corpus, which on Git Bash is slower than the whole selfcheck.
harness_hooks_dump() {
  for hh_hm in 1 ''; do
    for hh_ca in '' '--jdk-only'; do
      HAVE_MODULE="$hh_hm"; CRATONVM_ARGS="$hh_ca"
      for hh_h in class_args class_cp_extra class_cv_args; do
        for hh_c in $1 __no_such_vector__; do
          printf 'HAVE_MODULE=%s CRATONVM_ARGS=%s %s %s -> ' "$hh_hm" "$hh_ca" "$hh_h" "$hh_c"
          $hh_h "$hh_c"
          printf '\n'
        done
      done
    done
  done
}

# ---------------------------------------------------------------------------
# harness_hooks_drift <run.sh> <classes> <scratch-dir>
#
# The transitional ratchet for the duplication described above. Extracts run.sh's
# own copies of the three hooks, runs both definitions over the same class list
# and environment matrix, and reports any behavioural difference.
#
# THREE OUTCOMES, and the middle one is the point:
#   run.sh defines none  -> silent. The migration is complete and this function
#                           has nothing left to guard; it does not have to be
#                           deleted to stop firing.
#   copies agree         -> silent, and the selfcheck has just PROVEN that the
#                           configuration it guards vectors under is the
#                           configuration run.sh launches them under.
#   copies disagree      -> loud, naming the first differing answer. Somebody
#                           added an arm to one file.
#
# Appends to $HARNESS_GUARD_MSGS. Returns 1 on drift, 0 otherwise.
# ---------------------------------------------------------------------------
harness_hooks_drift() {
  hd_run="$1"; hd_classes="$2"; hd_dir="$3"
  [ -f "$hd_run" ] || return 0
  mkdir -p "$hd_dir" || return 1
  # Function bodies only, from `name() {` to the next line that is exactly `}`.
  awk '
    /^(class_args|class_cv_args|class_cp_extra)\(\) \{/ { inf = 1 }
    inf                                                 { print }
    inf && /^\}[[:space:]]*$/                           { inf = 0 }
  ' "$hd_run" > "$hd_dir/run-hooks.sh"
  hd_n=$(grep -acE '^(class_args|class_cv_args|class_cp_extra)\(\) \{' "$hd_dir/run-hooks.sh")
  hd_n=${hd_n:-0}
  [ "$hd_n" -eq 0 ] && return 0
  # Both dumps in SUBSHELLS: harness_hooks_dump assigns HAVE_MODULE and
  # CRATONVM_ARGS, and the caller's values must survive.
  ( harness_hooks_dump "$hd_classes" ) > "$hd_dir/shared.txt" 2>&1
  ( . "$hd_dir/run-hooks.sh"; harness_hooks_dump "$hd_classes" ) > "$hd_dir/runsh.txt" 2>&1
  if cmp -s "$hd_dir/shared.txt" "$hd_dir/runsh.txt"; then return 0; fi
  HARNESS_GUARD_MSGS="$HARNESS_GUARD_MSGS
  HARNESS ERROR [H1] the per-class launch hooks in $hd_run DISAGREE with the shared
    definitions in harness-guard.sh ($hd_n of 3 still defined there). The guards would then
    launch vectors in a configuration the suite does not use, which is how a correctly-wired
    vector gets reported as broken and a broken one as fine. First difference:
$(diff "$hd_dir/shared.txt" "$hd_dir/runsh.txt" | grep -a '^[<>]' | head -6 | sed 's/^/      /')
    Fix by DELETING run.sh's copies (they shadow the shared ones; the shared ones then take
    over unchanged), or by mirroring the new arm into harness-guard.sh."
  return 1
}
