#!/usr/bin/env bash
# Blast radius of the self-recursion arity guard: which suite classes contain a
# self-recursive method the direct-call route cannot serve?
#
# Before the guard, the FIRST such method in a process panicked the compiler
# thread and nothing compiled afterwards -- so "classes with >= 1 refusal" is
# also "classes that used to run fully interpreted from that point on". The
# refusal is invisible from Java, which is why it needs a counter at all.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'
export MSYS_NO_PATHCONV=1

HERE=/c/craton/cratonvm/apps/hib-suite-runner
JDKWIN="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
CV="${CV:?set CV}"
OUT="${1:?usage: selfrec-blast.sh <outdir>}"
COUNT="${COUNT:-120}"
START="${START:-0}"
CAP="${CAP:-150}"

mkdir -p "$OUT"
cd "$HERE" || exit 1
: > "$OUT/refusals.txt"
: > "$OUT/perclass.tsv"

mapfile -t CLASSES < <(sed -n "$((START+1)),$((START+COUNT))p" passed.txt)
echo "classes: ${#CLASSES[@]}"

i=0
for cls in "${CLASSES[@]}"; do
  [ -z "$cls" ] && continue
  i=$((i+1))
  log="$OUT/c.log"
  CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_JIT_COMPILED=1 timeout "$CAP" "$CV" \
      --java-home "$JDKWIN" --Xmx 1500m \
      -Duser.language=en -Duser.country=US -Djava.awt.headless=true \
      @common.args CratonRunner "$cls" > "$log" 2>&1
  n=$(grep -c "selfrec-refused" "$log")
  if [ "$n" -gt 0 ]; then
    grep -o "selfrec-refused .*" "$log" | sed 's/selfrec-refused //' | sort -u >> "$OUT/refusals.txt"
  fi
  printf '%s\t%s\n' "$n" "$cls" >> "$OUT/perclass.tsv"
  printf '%4d/%d  refusals=%-4s %s\n' "$i" "${#CLASSES[@]}" "$n" "${cls##*.}"
done

echo "=== classes with >=1 refusal ==="
awk -F'\t' '$1>0' "$OUT/perclass.tsv" | wc -l
echo "=== distinct refused methods ==="
sort -u "$OUT/refusals.txt" | tee "$OUT/methods.txt" | wc -l
echo DONE
