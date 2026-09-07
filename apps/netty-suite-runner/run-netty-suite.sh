#!/usr/bin/env bash
# =============================================================================
# run-netty-suite.sh — CratonVM Netty test-suite runner (Linux / Azure host)
#
# Runs Netty's JUnit 5 (Jupiter) test classes on CratonVM, fork-per-class (so a
# crash/hang in one class never stops the rest), with per-class timing and full
# logs persisted to disk.
#
# Same shape as apps/hib-suite-runner/run-hib.sh and the Windows-side
# apps/netty-suite-runner (see that dir's README for the prior Windows-only
# validation run):
#   - classes split into passed.txt (passes on CratonVM) / others.txt (everything
#     else: fail / hang / crash / loaderror)
#   - fork-per-class via CratonRunner (JUnit Platform Launcher), one VM per class
#   - sharded across parallel forks, with a flat per-class wall-clock timeout
#   - `categorize` sub-command rebuilds passed.txt/others.txt from a full run
#   - per-class accommodations via class-overrides.tsv (tracked in git)
#
# CratonRunner takes class names directly as argv (one VM per class here, for
# crash/hang isolation) and prints
#   @@RESULT <className> found=.. started=.. ok=.. failed=.. aborted=.. skipped=.. ms=..
# plus @@BATCHEND failed_classes=N. No list-file/start-index/@@DONE protocol.
#
# This is a CratonVM-only harness (no --hotspot baseline mode) for this phase.
#
# Netty specifics on this host:
#   - Maven multi-module reactor built with `./mvnw -fae -T 1C -DskipTests install`.
#   - netty-transport-native-epoll failed (hawtjni tries to download its own
#     not-yet-built native-src artifact from the sonatype snapshots repo, which
#     doesn't exist for this SNAPSHOT version — a build-tooling/network gap, not
#     a CratonVM issue). Its downstream native/OSGi-only modules were SKIPPED by
#     the reactor as a result: codec-native-quic, codec-http3, netty-all,
#     transport-native-io_uring, testsuite-native, testsuite-jpms,
#     testsuite-karaf, testsuite-osgi, testsuite-shading, microbench.
#     testlist.txt only contains classes discovered from modules that actually
#     produced test-classes.
#   - common.args' -cp is `<module>/target/classes` + `<module>/target/test-classes`
#     across every buildable module, plus every jar `dependency:build-classpath
#     -DincludeScope=test` resolved per module (see README.md for how it was built).
# =============================================================================
set -uo pipefail

# --- locations (Windows-form paths: the VM and JVM need native paths) --------
SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd)"
HERE="${HERE:-C:/craton/CratonVM/apps/netty-suite-runner}"
COMMON="$HERE/common.args"          # -cp + sysprops for the forked VM
RUNNER_CLASS="CratonRunner"         # compiled in $HERE, already on the classpath
OVERRIDES="${NETTY_CLASS_OVERRIDES:-}"
if [ -z "$OVERRIDES" ]; then
  if [ -f "$SELF_DIR/class-overrides.tsv" ]; then OVERRIDES="$SELF_DIR/class-overrides.tsv"
  else OVERRIDES="$HERE/class-overrides.tsv"; fi
fi
BENIGN_ABORTS="${NETTY_KNOWN_BENIGN_ABORTS:-}"
if [ -z "$BENIGN_ABORTS" ]; then
  if [ -f "$SELF_DIR/known-benign-aborts.tsv" ]; then BENIGN_ABORTS="$SELF_DIR/known-benign-aborts.tsv"
  else BENIGN_ABORTS="$HERE/known-benign-aborts.tsv"; fi
fi
NETTY_SRC="${NETTY_SRC:-C:/craton/CratonVM/apps/netty}"   # the built Maven reactor
MODSCOPE="${NETTY_MODULE_SCOPED:-}"
if [ -z "$MODSCOPE" ]; then
  if [ -f "$SELF_DIR/module-scoped-classes.tsv" ]; then MODSCOPE="$SELF_DIR/module-scoped-classes.tsv"
  else MODSCOPE="$HERE/module-scoped-classes.tsv"; fi
fi
MODARGS_DIR="$(dirname "$MODSCOPE")/module-args"
GEN_MODARGS="$(dirname "$MODSCOPE")/gen-module-args.sh"

# --- JDK autodetect ----------------------------------------------------------
detect_jdk() {
  local c
  for c in "C:/Program Files/Eclipse Adoptium"/jdk-25* \
           "C:/Program Files/Java"/jdk-25* \
           "C:/Program Files/Eclipse Adoptium"/jdk-2* \
           "C:/Program Files/Java"/jdk-2*; do
    [ -x "$c/bin/java.exe" ] && { printf '%s' "$c"; return 0; }
  done
  [ -x "/data/toolchain/jdk-25/bin/java" ] && { printf '%s' "/data/toolchain/jdk-25"; return 0; }
  return 1
}

# --- defaults (override via flags or env) ------------------------------------
CV_BIN="${CV_BIN:-C:/craton/CratonVM/target/release/cratonvm.exe}"
JDK="${JDK:-$(detect_jdk 2>/dev/null || echo "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot")}"
CV_XMX="${CV_XMX:-1500m}"
SHARDS="${SHARDS:-6}"               # parallel forks per mode
TIMEOUT="${TIMEOUT:-180}"           # per-class wall cap (s) -> HANG
OUTROOT="${OUTROOT:-$HERE/runs}"
USE_OVERRIDES=1                     # --no-overrides disables the table (A/B)
USE_MODSCOPE=0                      # module scope disabled by default on Windows

CATEGORY="passed"
JITMODE="on"
JDKMODE="real"
COUNT="0"                           # 0 = all from START to end
START="0"
ALLMODES="0"
GCMODE=""                           # --gc <default|g1|zgc>: adds -XX:+Use{G1,Z}GC (empty = engine default)

usage() {
  cat <<'USAGE'
run-netty-suite.sh — run the Netty test suite on CratonVM

USAGE:
  run-netty-suite.sh [options]

OPTIONS:
  --category <passed|others>   which list to run (default: passed)
  --count <N>                  number of classes to run (default: 0 = all)
  --start <IDX>                0-based start index into the category list (default: 0)
  --jit <on|off>               JIT on, or off via --nojit (default: on)
  --jdk <real|synthetic>       real-JDK (--java-home) or synthetic (--synthetic-jdk) (default: real)
  --all-modes                  run all 4 modes (jit{on,off} x jdk{real,synthetic}) concurrently,
                               each in its own background thread (ignores --jit/--jdk)
  --timeout <SEC>              per-class hang timeout (default: 180); per-class
                               overrides raise this as a floor, never lower it
  --no-overrides               ignore class-overrides.tsv entirely (A/B checks)
  --module-scope                run module-scoped-classes.tsv entries with their own Maven-faithful classpath (default: off)
  --no-module-scope            ignore module-scoped-classes.tsv; run those classes
                               on the flat whole-reactor classpath (A/B checks)
  --shards <N>                 parallel forks per mode (default: 6)
  --bin <path>                 cratonvm binary (default: $CV_BIN env or release build)
  --out <dir>                  output root (default: ./runs)
  --list <file>                run an explicit class-list file instead of a category
  -h | --help

SUB-COMMANDS:
  run-netty-suite.sh categorize   rebuild passed.txt / others.txt from a full run
  run-netty-suite.sh overrides    print the loaded per-class override table and exit
  run-netty-suite.sh module-scope print the module-scoped class table and exit
  run-netty-suite.sh benign-aborts print the known-benign-aborts table and exit

CATEGORIES (regenerate authoritatively with:  run-netty-suite.sh categorize):
  passed.txt  / others.txt    in this folder

KNOWN-BENIGN ABORTS:
  known-benign-aborts.tsv (tracked, next to this script) lists classes whose
  ABORTED status is a confirmed platform self-skip rather than a defect, with
  the exact found/ok/aborted counts that make it so. `categorize` treats such a
  class as pass-equivalent ONLY on an exact count match, so a changed abort
  profile still lands in others.txt. Check it with
  `run-netty-suite.sh benign-aborts`.

MODULE-SCOPED CLASSES:
  A handful of classes cannot run against the flat whole-reactor classpath and
  must see one module only, the way Maven shows it to them --
  NativeImageHandlerMetadataTest x 17, which compares the ChannelHandler
  subtypes it can reach against the module's checked-in native-image
  reflect-config.json. module-scoped-classes.tsv (tracked, next to this script)
  lists them with their module directory and Maven coordinates; each runs with
  the module dir as cwd and module-args/<artifactId>.args as its argfile,
  regenerated on demand by gen-module-args.sh. Every run prints
  `module-scope=N (loaded)`; check it with `run-netty-suite.sh module-scope`.

PER-CLASS OVERRIDES:
  class-overrides.tsv (tracked in git, next to this script) raises the wall cap
  and/or adds VM flags for individual classes that the flat default misjudges.
  Every run prints `overrides=N (loaded)` in its mode header; `overrides=0
  (MISSING ...)` means the table is gone. Check it with `run-netty-suite.sh overrides`.

ENV PASS-THROUGH:
  Any CRATONVM_* variable in your environment is inherited by the VM, e.g.
    CRATONVM_TIER_C2_THRESHOLD=200 run-netty-suite.sh --count 20
  Other knobs: CV_BIN, JDK, CV_XMX, SHARDS, TIMEOUT, OUTROOT

EXAMPLES:
  # small validation batch, first 15 classes of testlist.txt
  run-netty-suite.sh --list testlist.txt --count 15
  # first 200 passing classes, default (JIT on, real JDK)
  run-netty-suite.sh --category passed --count 200
  # rebuild passed.txt / others.txt from a fresh full run over testlist.txt
  run-netty-suite.sh categorize
USAGE
}

# --- per-class override table ------------------------------------------------
declare -A CLASS_TIMEOUT_OVERRIDE=()
declare -A CLASS_FLAGS_OVERRIDE=()
OVERRIDES_STATE="disabled"

load_overrides() {
  CLASS_TIMEOUT_OVERRIDE=(); CLASS_FLAGS_OVERRIDE=()
  if [ "$USE_OVERRIDES" != 1 ]; then OVERRIDES_STATE="disabled (--no-overrides)"; return 0; fi
  if [ ! -f "$OVERRIDES" ]; then
    OVERRIDES_STATE="MISSING $OVERRIDES"
    echo "WARNING: per-class override table not found: $OVERRIDES" >&2
    echo "WARNING: known-slow classes will be killed at the flat ${TIMEOUT}s cap and reported HANG." >&2
    return 0
  fi
  local cls to fl _rest
  while IFS=$'\t' read -r cls to fl _rest || [ -n "${cls:-}" ]; do
    cls="${cls%$'\r'}"; to="${to:-}"; to="${to%$'\r'}"; fl="${fl:-}"; fl="${fl%$'\r'}"
    case "$cls" in ''|\#*) continue;; esac
    if [ -n "$to" ] && [ "$to" != "-" ]; then
      case "$to" in
        *[!0-9]*|'') echo "WARNING: $OVERRIDES: non-numeric timeout '$to' for $cls — ignored" >&2;;
        *) CLASS_TIMEOUT_OVERRIDE["$cls"]="$to";;
      esac
    fi
    [ -n "$fl" ] && [ "$fl" != "-" ] && CLASS_FLAGS_OVERRIDE["$cls"]="$fl"
  done < "$OVERRIDES"
  local n=$(( ${#CLASS_TIMEOUT_OVERRIDE[@]} > ${#CLASS_FLAGS_OVERRIDE[@]} ? ${#CLASS_TIMEOUT_OVERRIDE[@]} : ${#CLASS_FLAGS_OVERRIDE[@]} ))
  OVERRIDES_STATE="$n (loaded)"
}

print_overrides() {
  echo "override table: $OVERRIDES"
  echo "state: $OVERRIDES_STATE"
  local k
  for k in "${!CLASS_TIMEOUT_OVERRIDE[@]}"; do
    printf '  %s  timeout=%ss flags=%s\n' "$k" "${CLASS_TIMEOUT_OVERRIDE[$k]}" "${CLASS_FLAGS_OVERRIDE[$k]:--}"
  done
  for k in "${!CLASS_FLAGS_OVERRIDE[@]}"; do
    [ -n "${CLASS_TIMEOUT_OVERRIDE[$k]:-}" ] && continue
    printf '  %s  timeout=-  flags=%s\n' "$k" "${CLASS_FLAGS_OVERRIDE[$k]}"
  done
}

# --- known-benign-aborts table: confirmed HotSpot-parity ABORTED classes -----
# `categorize` treats a listed class as pass-equivalent only when its
# found/ok/aborted counts match this table EXACTLY, so a future regression that
# shifts the abort profile still lands in others.txt like any other residual.
# See the table's own header for format and rationale.
declare -A BENIGN_FOUND=() BENIGN_OK=() BENIGN_ABORTED=()
BENIGN_STATE="not loaded"

load_benign_aborts() {
  BENIGN_FOUND=(); BENIGN_OK=(); BENIGN_ABORTED=()
  if [ ! -f "$BENIGN_ABORTS" ]; then
    BENIGN_STATE="MISSING $BENIGN_ABORTS"
    echo "WARNING: known-benign-aborts table not found: $BENIGN_ABORTS" >&2
    echo "WARNING: confirmed-benign ABORTED classes will be re-flagged into others.txt by the next categorize run." >&2
    return 0
  fi
  local cls ef eo ea _rest
  while IFS=$'\t' read -r cls ef eo ea _rest || [ -n "${cls:-}" ]; do
    cls="${cls%$'\r'}"; ef="${ef%$'\r'}"; eo="${eo%$'\r'}"; ea="${ea%$'\r'}"
    case "$cls" in ''|\#*) continue;; esac
    BENIGN_FOUND["$cls"]="$ef"; BENIGN_OK["$cls"]="$eo"; BENIGN_ABORTED["$cls"]="$ea"
  done < "$BENIGN_ABORTS"
  BENIGN_STATE="${#BENIGN_FOUND[@]} (loaded)"
}

print_benign_aborts() {
  echo "known-benign-aborts table: $BENIGN_ABORTS"
  echo "state: $BENIGN_STATE"
  local k
  for k in "${!BENIGN_FOUND[@]}"; do
    printf '  %s  found=%s ok=%s aborted=%s\n' "$k" "${BENIGN_FOUND[$k]}" "${BENIGN_OK[$k]}" "${BENIGN_ABORTED[$k]}"
  done
}

# --- module-scoped class table ----------------------------------------------
# Classes that must NOT see the flat whole-reactor classpath. Each entry pins a
# working directory (the module dir) and an argfile carrying that one module's
# Maven test classpath plus the groupId/artifactId system properties surefire
# would have set. Without this, all 17 NativeImageHandlerMetadataTest classes
# fail identically on CratonVM and on stock HotSpot -- see
# fixed-suite-bugs/netty/nativeimagehandlermetadatatest-harness-module-scope-FIXED-20260819.md
declare -A CLASS_WORKDIR=()
declare -A CLASS_ARGFILE=()
MODSCOPE_STATE="disabled"

load_module_scope() {
  CLASS_WORKDIR=(); CLASS_ARGFILE=()
  if [ "$USE_MODSCOPE" != 1 ]; then MODSCOPE_STATE="disabled (--no-module-scope)"; return 0; fi
  if [ ! -f "$MODSCOPE" ]; then
    MODSCOPE_STATE="MISSING $MODSCOPE"
    echo "WARNING: module-scoped class table not found: $MODSCOPE" >&2
    echo "WARNING: NativeImageHandlerMetadataTest x17 will FAIL on the flat classpath (on HotSpot too)." >&2
    return 0
  fi
  local cls mod grp art _rest missing=0
  while IFS=$'\t' read -r cls mod grp art _rest || [ -n "${cls:-}" ]; do
    cls="${cls%$'\r'}"; art="${art:-}"; art="${art%$'\r'}"; mod="${mod:-}"; mod="${mod%$'\r'}"
    case "$cls" in ''|\#*) continue;; esac
    [ -n "$mod" ] && [ -n "$art" ] || continue
    CLASS_WORKDIR["$cls"]="$NETTY_SRC/$mod"
    CLASS_ARGFILE["$cls"]="$MODARGS_DIR/$art.args"
    [ -f "$MODARGS_DIR/$art.args" ] || missing=$((missing+1))
  done < "$MODSCOPE"
  # The argfiles hold host-specific absolute paths, so they are generated, not
  # tracked. Rebuild them the first time they are needed rather than silently
  # running these classes wrong.
  if [ "$missing" -gt 0 ] && [ -x "$GEN_MODARGS" ]; then
    echo "[module-scope] $missing argfile(s) missing under $MODARGS_DIR — running gen-module-args.sh" >&2
    "$GEN_MODARGS" >&2 || echo "WARNING: gen-module-args.sh failed; some classes keep the flat classpath" >&2
  fi
  local ok=0 gone=0 k
  for k in "${!CLASS_ARGFILE[@]}"; do
    if [ -f "${CLASS_ARGFILE[$k]}" ] && [ -d "${CLASS_WORKDIR[$k]}" ]; then ok=$((ok+1)); else
      gone=$((gone+1)); unset 'CLASS_ARGFILE[$k]'; unset 'CLASS_WORKDIR[$k]'
    fi
  done
  if [ "$gone" -eq 0 ]; then MODSCOPE_STATE="$ok (loaded)"; else MODSCOPE_STATE="$ok (loaded, $gone unusable)"; fi
}

print_module_scope() {
  echo "module-scoped table: $MODSCOPE"
  echo "netty source tree:   $NETTY_SRC"
  echo "argfile dir:         $MODARGS_DIR"
  echo "state: $MODSCOPE_STATE"
  local k
  for k in "${!CLASS_ARGFILE[@]}"; do
    printf '  %s\n    cwd=%s\n    args=%s\n' "$k" "${CLASS_WORKDIR[$k]}" "${CLASS_ARGFILE[$k]}"
  done
}

# --- categorize sub-command: (re)build passed.txt / others.txt ----------------
if [ "${1:-}" = "categorize" ]; then
  echo "[categorize] running the full testlist once (JIT on, real JDK) to split passed/others ..."
  CATEGORIZE=1
else
  CATEGORIZE=0
fi

# --- arg parse ---------------------------------------------------------------
SHOW_OVERRIDES=0
SHOW_MODSCOPE=0
SHOW_BENIGN=0
EXPLICIT_LIST=""
while [ $# -gt 0 ]; do
  case "$1" in
    categorize) shift;;
    overrides)  SHOW_OVERRIDES=1; shift;;
    module-scope) SHOW_MODSCOPE=1; shift;;
    benign-aborts) SHOW_BENIGN=1; shift;;
    --no-overrides) USE_OVERRIDES=0; shift;;
    --module-scope)    USE_MODSCOPE=1; shift;;
    --no-module-scope) USE_MODSCOPE=0; shift;;
    --category) CATEGORY="$2"; shift 2;;
    --count)    COUNT="$2"; shift 2;;
    --start)    START="$2"; shift 2;;
    --jit)      JITMODE="$2"; shift 2;;
    --jdk)      JDKMODE="$2"; shift 2;;
    --gc)       GCMODE="$2"; shift 2;;
    --all-modes) ALLMODES=1; shift;;
    --timeout)  TIMEOUT="$2"; shift 2;;
    --shards)   SHARDS="$2"; shift 2;;
    --bin)      CV_BIN="$2"; shift 2;;
    --out)      OUTROOT="$2"; shift 2;;
    --list)     EXPLICIT_LIST="$2"; shift 2;;
    -h|--help)  usage; exit 0;;
    *) echo "unknown option: $1" >&2; usage; exit 2;;
  esac
done

load_overrides
if [ "$SHOW_OVERRIDES" = 1 ]; then print_overrides; exit 0; fi
load_module_scope
if [ "$SHOW_MODSCOPE" = 1 ]; then print_module_scope; exit 0; fi
load_benign_aborts
if [ "$SHOW_BENIGN" = 1 ]; then print_benign_aborts; exit 0; fi

ORIG_PWD="$PWD"
abspath() { case "$1" in /*|[A-Za-z]:[/\\]*) printf '%s' "$1";; *) printf '%s/%s' "$ORIG_PWD" "$1";; esac; }
CV_BIN="$(abspath "$CV_BIN")"
OUTROOT="$(abspath "$OUTROOT")"
[ -n "$EXPLICIT_LIST" ] && EXPLICIT_LIST="$(abspath "$EXPLICIT_LIST")"
cd "$HERE" || { echo "ERROR: cannot cd to fixture dir: $HERE" >&2; exit 1; }

[ -f "$CV_BIN" ] || { echo "ERROR: cratonvm binary not found: $CV_BIN (set --bin or CV_BIN)" >&2; exit 1; }
[ -f "$COMMON" ] || { echo "ERROR: common.args not found: $COMMON" >&2; exit 1; }
[ -x "$JDK/bin/java.exe" ] || [ -x "$JDK/bin/java" ] || { echo "ERROR: real JDK not found: '${JDK:-<none detected>}' (set JDK=... env)" >&2; exit 1; }

# Reactor-built sanity check (2026-09-04): a missing directory on a Java
# classpath is silently skipped, not an error, so an unbuilt Maven reactor
# does not fail loudly here -- every class whose module target/ dir is
# missing just reports "found=0 tests" and the run finishes looking clean.
# 683/733 classes on this exact list silently reported 0 tests this way
# after a checkout refresh wiped every module's target/ dir with nothing
# rebuilding it; only netty-transport's classes ran, because its OWN stale
# test-jar happened to be pulled into common.args as a transitive Maven
# dependency. Count how many `*/target/classes` entries in common.args'
# flat classpath actually exist as directories; refuse to run if most are
# missing, rather than produce a summary that reads as a completed suite.
_cp_line="$(sed -n '2p' "$COMMON")"
_target_total=0; _target_present=0
IFS=':' read -ra _cp_entries <<< "$_cp_line"
for _e in "${_cp_entries[@]}"; do
  case "$_e" in
    */target/classes)
      _target_total=$((_target_total + 1))
      [ -d "$_e" ] && _target_present=$((_target_present + 1))
      ;;
  esac
done
if [ "$_target_total" -gt 0 ]; then
  _target_pct=$(( _target_present * 100 / _target_total ))
  if [ "$_target_pct" -lt 50 ]; then
    echo "ERROR: the Netty Maven reactor looks unbuilt: only $_target_present/$_target_total" >&2
    echo "       module target/classes directories exist (common.args: $COMMON)." >&2
    echo "       Running anyway would silently report found=0 for most classes" >&2
    echo "       instead of a real result. Build the reactor first:" >&2
    echo "         cd $NETTY_SRC && JAVA_HOME=<a full JDK, not just a JRE> \\" >&2
    echo "           ./mvnw -fae -T 1C -DskipTests -Dcheckstyle.skip=true \\" >&2
    echo "           -Dlicense.skip=true install -Dmaven.repo.local=<your m2 repo>" >&2
    echo "       (netty-transport-native-epoll may still fail on a network-restricted" >&2
    echo "       host -- a missing native-src download, unrelated to pure-Java tests" >&2
    echo "       and safe to ignore with -fae.)" >&2
    exit 1
  fi
fi
mkdir -p "$OUTROOT"
TS="$(date +%Y%m%d-%H%M%S)"

# --- fork-per-class runner for one shard -------------------------------------
run_shard() {
  local LIST="$1" OUT="$2"
  mkdir -p "$OUT"
  local RAW="$OUT/raw.log" TSV="$OUT/results.tsv"
  : > "$RAW"
  printf 'idx\tclass\tstatus\tfound\tok\tfailed\taborted\tskipped\tms\tsig\n' > "$TSV"
  local idx=0 cls tmp rc
  while IFS= read -r cls; do
    [ -z "$cls" ] && continue
    local cls_to cls_fl eff_to f
    local -a eff_flags=("${VMFLAGS_BASE[@]}")
    cls_to="${CLASS_TIMEOUT_OVERRIDE[$cls]:-}"
    cls_fl="${CLASS_FLAGS_OVERRIDE[$cls]:-}"
    eff_to="$TIMEOUT"
    if [ -n "$cls_to" ] && [ "$cls_to" -gt "$TIMEOUT" ] 2>/dev/null; then eff_to="$cls_to"; fi
    if [ -n "$cls_fl" ]; then
      for f in $cls_fl; do
        case " ${eff_flags[*]} " in *" $f "*) ;; *) eff_flags+=("$f");; esac
      done
    fi
    if [ -n "$cls_to" ] || [ -n "$cls_fl" ]; then
      echo "[override] $cls timeout=${eff_to}s (default ${TIMEOUT}s) extra_flags=${cls_fl:--}" >> "$RAW"
      echo "[override] $cls timeout=${eff_to}s extra_flags=${cls_fl:--}" >> "$OUT/../overrides.log"
    fi
    # A module-scoped class swaps the flat whole-reactor argfile for its own
    # module's Maven test classpath and runs from the module directory: the
    # resource path it checks is relative to the process working directory.
    local cls_args cls_dir
    cls_args="${CLASS_ARGFILE[$cls]:-$COMMON}"
    cls_dir="${CLASS_WORKDIR[$cls]:-}"
    eff_flags+=(@"$cls_args")

    tmp=$(mktemp)
    if [ -n "$cls_dir" ]; then
      echo "[module-scope] $cls cwd=$cls_dir args=$cls_args" >> "$RAW"
      ( cd "$cls_dir" && CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$eff_to" "$CV_BIN" "${eff_flags[@]}" \
          -Dcraton.batch=1 "$RUNNER_CLASS" "$cls" ) >"$tmp" 2>>"$RAW"; rc=$?
    else
      CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$eff_to" "$CV_BIN" "${eff_flags[@]}" \
          -Dcraton.batch=1 "$RUNNER_CLASS" "$cls" >"$tmp" 2>>"$RAW"; rc=$?
    fi
    cat "$tmp" >> "$RAW"
    local rline found ok failed aborted skipped ms status sig
    rline=$(grep "^@@RESULT " "$tmp" | head -1)
    if [ -n "$rline" ]; then
      found=$(printf '%s' "$rline"|grep -o 'found=[0-9]*'|cut -d= -f2); ok=$(printf '%s' "$rline"|grep -o 'ok=[0-9]*'|cut -d= -f2)
      failed=$(printf '%s' "$rline"|grep -o 'failed=[0-9]*'|cut -d= -f2); aborted=$(printf '%s' "$rline"|grep -o 'aborted=[0-9]*'|cut -d= -f2)
      skipped=$(printf '%s' "$rline"|grep -o 'skipped=[0-9]*'|cut -d= -f2); ms=$(printf '%s' "$rline"|grep -o 'ms=[0-9]*'|cut -d= -f2)
      status=PASS
      if [ "${failed:-0}" -gt 0 ]; then status=FAIL
      elif [ "${aborted:-0}" -gt 0 ]; then status=ABORTED
      elif [ "${found:-0}" -eq 0 ]; then status=NOTESTS; fi
      sig=""
      if [ "$status" = FAIL ]; then sig=$(grep -m1 -E "^(MethodSource|[A-Za-z][A-Za-z0-9_.]*(Exception|Error))" "$tmp" | head -c 160); fi
      printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$idx" "$cls" "$status" "${found:-0}" "${ok:-0}" "${failed:-0}" "${aborted:-0}" "${skipped:-0}" "${ms:-0}" "$sig" >> "$TSV"
    else
      local st; if [ "$rc" -eq 124 ]; then st=HANG; else st=CRASH; fi
      printf '%s\t%s\t%s\t0\t0\t0\t0\t0\t0\t%s rc=%s timeout=%ss\n' "$idx" "$cls" "$st" "process-died" "$rc" "$eff_to" >> "$TSV"
    fi
    rm -f "$tmp"
    idx=$((idx+1))
  done < "$LIST"
}

# --- run one mode (sharded) --------------------------------------------------
run_mode() {
  local label="$1" jit="$2" jdk="$3" SLICE="$4" MODE="$5"
  mkdir -p "$MODE"
  VMFLAGS_BASE=(--java-home "$JDK" --Xmx "$CV_XMX")
  [ "$jit" = off ] && VMFLAGS_BASE+=(--nojit)
  [ "$jdk" = synthetic ] && VMFLAGS_BASE+=(--synthetic-jdk)
  case "$GCMODE" in
    g1)  VMFLAGS_BASE+=(-XX:+UseG1GC);;
    zgc) VMFLAGS_BASE+=(-XX:+UseZGC);;
    generational|gen) VMFLAGS_BASE+=(-XX:+UseGenerationalGC);;
    default|"") ;;
    *) echo "ERROR: unknown --gc mode: $GCMODE (want default|g1|zgc|generational)" >&2; exit 2;;
  esac
  local n; n=$(grep -c '' "$SLICE")
  echo "[$label] $n classes | jit=$jit jdk=$jdk gc=${GCMODE:-default} shards=$SHARDS timeout=${TIMEOUT}s overrides=$OVERRIDES_STATE module-scope=$MODSCOPE_STATE bin=$CV_BIN"
  local t0; t0=$(date +%s)
  local s pids=()
  for ((s=0; s<SHARDS; s++)); do awk -v n="$SHARDS" -v r="$s" 'NR%n==r' "$SLICE" > "$MODE/shard-$s.txt"; done
  for ((s=0; s<SHARDS; s++)); do ( run_shard "$MODE/shard-$s.txt" "$MODE/shard-$s" ) & pids+=($!); done
  for p in "${pids[@]}"; do wait "$p"; done
  local t1; t1=$(date +%s); local secs=$((t1-t0))
  local MERGED="$MODE/results.tsv"
  head -1 "$MODE/shard-0/results.tsv" > "$MERGED" 2>/dev/null
  for ((s=0; s<SHARDS; s++)); do tail -n +2 "$MODE/shard-$s/results.tsv" 2>/dev/null; done >> "$MERGED"
  local rec; rec=$(( $(grep -c '' "$MERGED") - 1 ))
  {
    echo "mode=$label jit=$jit jdk=$jdk gc=${GCMODE:-default} classes=$n recorded=$rec wall_seconds=$secs ($((secs/60))m$((secs%60))s) overrides=$OVERRIDES_STATE module-scope=$MODSCOPE_STATE"
    awk -F'\t' 'NR>1{c[$3]++; tms+=$9} END{printf "status:"; for(k in c) printf " %s=%d",k,c[k]; printf "  sum_class_ms=%d\n",tms}' "$MERGED"
  } | tee "$MODE/SUMMARY.txt"
}

# --- resolve category list ---------------------------------------------------
if [ -n "$EXPLICIT_LIST" ]; then
  SRCLIST="$EXPLICIT_LIST"
elif [ "$CATEGORIZE" = 1 ]; then
  SRCLIST="$HERE/testlist.txt"
else
  case "$CATEGORY" in
    passed) SRCLIST="$HERE/passed.txt";;
    others|failed) SRCLIST="$HERE/others.txt";;
    *) echo "bad --category: $CATEGORY" >&2; exit 2;;
  esac
fi
[ -f "$SRCLIST" ] || { echo "ERROR: list not found: $SRCLIST" >&2; exit 1; }

# --- slice [START, START+COUNT) ----------------------------------------------
SLICE="$OUTROOT/.slice-$TS.txt"
if [ "$COUNT" -gt 0 ]; then
  awk -v s="$START" -v c="$COUNT" 'NR>s && NR<=s+c' "$SRCLIST" > "$SLICE"
else
  awk -v s="$START" 'NR>s' "$SRCLIST" > "$SLICE"
fi
SLN=$(grep -c '' "$SLICE")
echo "=== run-netty-suite $TS :: category=$CATEGORY start=$START count=$COUNT -> $SLN classes ==="

# --- categorize: run full list (jit on/real), then split ---------------------
if [ "$CATEGORIZE" = 1 ]; then
  RUN="$OUTROOT/categorize-$TS"
  run_mode "categorize" on real "$SLICE" "$RUN"
  # Split by literal status, except that a class listed in known-benign-aborts.tsv
  # whose found/ok/aborted counts match that table EXACTLY is pass-equivalent: its
  # aborts are platform self-skips that can never pass here, and re-flagging them
  # into others.txt every round buys a fresh investigation and nothing else.
  : > "$HERE/passed.txt"; : > "$HERE/others.txt"
  RECLASSIFIED=0
  { read -r _hdr
    while IFS=$'\t' read -r _idx rcls rstatus rfound rok _rfailed rabort _rest; do
      [ -z "${rcls:-}" ] && continue
      if [ "$rstatus" = "PASS" ]; then
        echo "$rcls" >> "$HERE/passed.txt"
      elif [ "$rstatus" = "ABORTED" ] && [ -n "${BENIGN_FOUND[$rcls]:-}" ] \
           && [ "$rfound" = "${BENIGN_FOUND[$rcls]}" ] \
           && [ "$rok" = "${BENIGN_OK[$rcls]}" ] \
           && [ "$rabort" = "${BENIGN_ABORTED[$rcls]}" ]; then
        echo "$rcls" >> "$HERE/passed.txt"
        RECLASSIFIED=$((RECLASSIFIED+1))
      else
        echo "$rcls" >> "$HERE/others.txt"
      fi
    done
  } < "$RUN/results.tsv"
  sort -u -o "$HERE/passed.txt" "$HERE/passed.txt"
  sort -u -o "$HERE/others.txt" "$HERE/others.txt"
  echo "rebuilt passed.txt=$(grep -c '' "$HERE/passed.txt")  others.txt=$(grep -c '' "$HERE/others.txt")  known-benign-aborts=$BENIGN_STATE reclassified=$RECLASSIFIED"
  rm -f "$SLICE"; exit 0
fi

# --- single mode or all 4 modes concurrently ---------------------------------
RUN="$OUTROOT/run-$TS-$CATEGORY"
mkdir -p "$RUN"
echo "output: $RUN"
GT0=$(date +%s)
if [ "$ALLMODES" = 1 ]; then
  declare -a MPIDS=()
  run_mode "jiton-real"  on  real      "$SLICE" "$RUN/jiton-real"  > "$RUN/jiton-real.log"  2>&1 &  MPIDS+=($!)
  run_mode "jitoff-real" off real      "$SLICE" "$RUN/jitoff-real" > "$RUN/jitoff-real.log" 2>&1 &  MPIDS+=($!)
  run_mode "jiton-syn"   on  synthetic "$SLICE" "$RUN/jiton-syn"   > "$RUN/jiton-syn.log"   2>&1 &  MPIDS+=($!)
  run_mode "jitoff-syn"  off synthetic "$SLICE" "$RUN/jitoff-syn"  > "$RUN/jitoff-syn.log"  2>&1 &  MPIDS+=($!)
  echo "launched 4 modes concurrently (pids: ${MPIDS[*]}); logs in $RUN/<mode>.log"
  for p in "${MPIDS[@]}"; do wait "$p"; done
  echo "=== ALL-MODES SUMMARY ==="; for m in jiton-real jitoff-real jiton-syn jitoff-syn; do cat "$RUN/$m/SUMMARY.txt" 2>/dev/null; done
else
  run_mode "$JITMODE-$JDKMODE" "$JITMODE" "$JDKMODE" "$SLICE" "$RUN/$JITMODE-$JDKMODE"
fi
GT1=$(date +%s)
echo "=== TOTAL wall: $(( (GT1-GT0)/60 ))m$(( (GT1-GT0)%60 ))s | results under $RUN ==="
rm -f "$SLICE"
