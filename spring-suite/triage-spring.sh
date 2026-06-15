#!/usr/bin/env bash
# Phase 2+3: for every class whose CratonVM status is SUSPECT (not OK/EMPTY),
# run it under real HotSpot JDK 25 to decide whether the failure is CratonVM-
# UNIQUE. Only CV-unique crashes/hangs/loaderrs/correctness-fails are reported;
# anything HotSpot also fails is "same behaviour" and dropped. Emits:
#   triage.tsv         class<TAB>cv_status<TAB>hs_status<TAB>verdict
#   comparison.md      human summary
#   cv-unique.tsv      the bug list (class, cv_status, hs_status, category)
set -u
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
SPRING="/c/craton/cratonvm/apps/spring-framework"
HARNESS="/c/craton/CratonVM-spring/spring-suite"
JDK25_W="C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot"
HS="$JDK25_W/bin/java.exe"
CVOUT="${CVOUT:-$HARNESS/results-cv}"
RES="$CVOUT/results.tsv"
OUT="${OUT:-$HARNESS/triage}"; mkdir -p "$OUT"
TR="$OUT/triage.tsv"; UNIQ="$OUT/cv-unique.tsv"; MD="$OUT/comparison.md"
HS_TO="${HS_TO:-180}"
KRUN_W=$(cygpath -m "$HARNESS")
# Resumable: keep prior triage.tsv/cv-unique.tsv, skip classes already triaged.
touch "$TR" "$UNIQ"
declare -A DONE
while IFS=$'\t' read -r c rest; do [ -n "$c" ] && DONE["$c"]=1; done < "$TR"

# Build class -> module-argfile index (regenerate per-module classpaths/argfiles)
declare -A CLS2AF
mapfile -t TESTDIRS < <(find "$SPRING" -type d -path '*/build/classes/*/test' 2>/dev/null | sort -u)
declare -A MODS; for td in "${TESTDIRS[@]}"; do MODS["${td%/build/classes/*}"]=1; done
for MOD in "${!MODS[@]}"; do
  MODNAME=$(basename "$MOD"); CPF="$MOD/build/cratonvm-testcp.txt"
  [ -f "$CPF" ] || continue
  MCP="$KRUN_W;$(tr -d '\r' < "$CPF")"
  AF="$OUT/.af_$MODNAME.txt"; { echo "-cp"; echo "$MCP"; } > "$AF"
  AFM=$(cygpath -m "$AF")
  while IFS= read -r c; do CLS2AF["$c"]="$AFM"; done < <(
    for d in "$MOD"/build/classes/*/test; do [ -d "$d" ] && (cd "$d" &&
      find . \( -name '*Tests.class' -o -name '*Test.class' \) ! -name '*$*' \
      | sed 's|^\./||; s|\.class$||; s|/|.|g'); done | sort -u)
done

hs_status() {  # $1 class $2 argfile -> echo STATUS  (parses KRun RESULT)
  local cls="$1" af="$2" raw rc line
  raw=$(timeout "$HS_TO" "$HS" "@$af" KRun "$cls" 2>/dev/null); rc=$?
  if [ $rc -eq 124 ]; then echo TIMEOUT; return; fi
  line=$(printf '%s\n' "$raw" | sed -n 's/^RESULT [^ ]* .*status=\([A-Z]*\).*/\1/p' | head -1)
  if [ -n "$line" ]; then echo "$line"; else echo ABEND; fi
}

# Iterate suspect CV classes (skip ones already triaged for resume)
total=0; reported=0
while IFS=$'\t' read -r cls cvst found succ fail skip abort; do
  case "$cvst" in OK|EMPTY|status) continue;; esac   # only suspects
  [ -n "${DONE[$cls]:-}" ] && continue
  af="${CLS2AF[$cls]:-}"
  cat="-"
  if [ -z "$af" ]; then printf '%s\t%s\tNOAF\tSKIP(no-classpath)\t-\n' "$cls" "$cvst" >> "$TR"; continue; fi
  total=$((total+1))
  hs=$(hs_status "$cls" "$af")
  if [ "$hs" = OK ] || [ "$hs" = EMPTY ]; then
    case "$cvst" in
      CRASH|ABEND)   cat="VM-CRASH";;
      TIMEOUT)       cat="VM-HANG";;
      LOADERR)       cat="VM-LOADERR";;
      FAIL)          cat="VM-CORRECTNESS";;
      *)             cat="VM-OTHER";;
    esac
    verdict="CV-UNIQUE"; reported=$((reported+1))
  else
    verdict="SAME-AS-HS"
  fi
  printf '%s\t%s\t%s\t%s\t%s\n' "$cls" "$cvst" "$hs" "$verdict" "$cat" >> "$TR"
  printf '  %-70s cv=%-8s hs=%-8s %s\n' "$cls" "$cvst" "$hs" "$verdict"
done < "$RES"

# Rebuild cv-unique.tsv from the full triage.tsv (idempotent across resumes)
awk -F'\t' '$4=="CV-UNIQUE"{printf "%s\t%s\t%s\t%s\n",$1,$2,$3,$5}' "$TR" | sort -u > "$UNIQ"
reported=$(wc -l < "$UNIQ")

# ---- report ----
{
  echo "# Spring Framework suite — CratonVM vs HotSpot JDK 25"
  echo
  echo "**CratonVM HEAD:** $(cd /c/craton/CratonVM && git rev-parse --short HEAD 2>/dev/null)"
  echo "**Suspect classes triaged against HotSpot:** $total"
  echo "**CratonVM-unique failures (HotSpot OK):** $reported"
  echo
  echo "## CratonVM totals (whole suite)"
  echo '```'
  cat "$CVOUT/summary.txt" 2>/dev/null
  echo '```'
  echo
  echo "## CratonVM-unique failures by category"
  echo
  echo "| Category | Count |"
  echo "|----------|-------|"
  awk -F'\t' '{c[$4]++} END{for(k in c) printf "| %s | %d |\n", k, c[k]}' "$UNIQ" | sort
  echo
  echo "## CratonVM-unique failures (detail)"
  echo
  echo "| Test class | CratonVM | HotSpot | Category |"
  echo "|-----------|----------|---------|----------|"
  sort "$UNIQ" | awk -F'\t' '{printf "| %s | %s | %s | %s |\n",$1,$2,$3,$4}'
} > "$MD"

echo
echo "==== triage done: $reported CV-unique of $total suspect ===="
echo "wrote $MD"
echo "wrote $UNIQ"
