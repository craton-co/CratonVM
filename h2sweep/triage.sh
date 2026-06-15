#!/usr/bin/env bash
# Triage a CratonVM sweep against the HotSpot baseline of the same config.
# Usage: triage.sh <craton-dir> <hotspot-dir>
#   e.g. triage.sh craton-mem1 hotspot-base
set -u
SW="C:/craton/CratonVM-h2suite/h2sweep"
CD="$SW/$1"; HD="$SW/$2"
CR="$CD/results.csv"; HR="$HD/results.csv"

# HotSpot status map
declare -A HS
while IFS=, read -r cls st rest; do HS["$cls"]="$st"; done < <(tail -n +2 "$HR" | tr -d '\000')

extract_sig() {
  # pull the most informative single line from a class's logs
  local short="$1"
  local out="$CD/$short.out.log" err="$CD/$short.err.log"
  # Prefer a FAIL/exception line from out, else a craton-internal signature from err
  local line
  line=$(cat "$out" 2>/dev/null | tr -d '\000' | grep -aoE '(FAIL|Caused by:|Exception|Error)[^\r\n]*' | head -1)
  if [ -z "$line" ]; then
    line=$(cat "$err" 2>/dev/null | tr -d '\000' | grep -aiE 'runtime error:|not implemented:|panicked|EXCEPTION_ACCESS_VIOLATION|ArrayStore|ClassCast|NullPointer|assert|unreachable|overflow|index out of|No such|cannot|stack dump' | grep -avi 'WARN ' | head -1)
  fi
  echo "$line" | sed 's/[[:space:]]\+/ /g' | cut -c1-160
}

echo "=== DIVERGENCES (CratonVM worse than HotSpot) ==="
printf "%-8s %-7s %-50s %s\n" "CRATON" "HS" "CLASS" "SIGNATURE"
tail -n +2 "$CR" | tr -d '\000' | while IFS=, read -r cls st rc sec sig; do
  hs="${HS[$cls]:-MISSING}"
  short="${cls##*.}"
  # report when craton is worse: craton in {FAIL,CRASH,HANG} and hotspot==PASS
  if [[ "$st" =~ ^(FAIL|CRASH|HANG)$ && "$hs" == "PASS" ]]; then
    printf "%-8s %-7s %-50s %s\n" "$st" "$hs" "$cls" "$(extract_sig "$short")"
  fi
done

echo
echo "=== SHARED (CratonVM not-PASS but HotSpot also not-PASS — excluded) ==="
tail -n +2 "$CR" | tr -d '\000' | while IFS=, read -r cls st rc sec sig; do
  hs="${HS[$cls]:-MISSING}"
  if [[ "$st" =~ ^(FAIL|CRASH|HANG)$ && "$hs" != "PASS" ]]; then
    printf "%-8s %-7s %s\n" "$st" "$hs" "$cls"
  fi
done

echo
echo "=== CRATON COUNTS ==="; tail -n +2 "$CR" | tr -d '\000' | cut -d, -f2 | sort | uniq -c
