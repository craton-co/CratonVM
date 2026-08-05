#!/bin/bash
# The page says ZipContentTests fails ONLY with the JIT on ("PASSes with
# --nojit at the same heap"), and attributes it to a larger JIT-arm footprint.
# A --nojit run has now failed once in three - in nestedZip64CanBeRead, the
# very test the page names - with a corrupt generated zip rather than an OOM.
# If --nojit fails at any rate at all, "JIT-only" is wrong and the footprint
# story goes with it. This measures the rate on each arm instead of arguing.
exec > /tmp/zip64-results.txt 2>&1
SB=/data/data/springboot-jsonreader-deprecation-20260718
JDK=/data/jdk25-real-20260717/jdk-25.0.3+9
CP="$SB/sb-runner:$(cat $SB/loader/spring-boot-loader/build/cratonvm-test-cp.txt)"
cd "$SB/loader/spring-boot-loader" || exit 9
echo "host: $(cat /proc/loadavg)  free: $(free -g | awk '/Mem:/{print $7}')G  /tmp: $(df -h /tmp | tail -1 | awk '{print $4}') free"

one() {  # one <label> <bin> <extra>
  local label="$1" bin="$2" extra="$3" out rc res f cf t why v
  out=$(timeout 900 "$bin" --java-home "$JDK" --Xmx 2g $extra \
          -Dfile.encoding=UTF-8 -cp "$CP" OneMethodRunner \
          org.springframework.boot.loader.zip.ZipContentTests nestedZip64CanBeRead 2>&1)
  rc=$?
  res=$(echo "$out" | grep -o 'SBRUNNER_RESULT.*' | head -1)
  why=$(echo "$out" | grep -oE "OutOfMemoryError.{0,40}|Zip64 .{0,60}|IOException.{0,50}" | head -1)
  f=$(echo "$res"  | grep -oE '(^| )failed=[0-9]+'      | grep -oE '[0-9]+')
  cf=$(echo "$res" | grep -oE 'containersFailed=[0-9]+' | grep -oE '[0-9]+')
  t=$(echo "$res"  | grep -oE 'tests=[0-9]+'            | grep -oE '[0-9]+')
  if   [ "$rc" = "124" ];   then v=TIMEOUT
  elif [ -z "$res" ];       then v="NORESULT rc=$rc"
  elif [ "${t:-0}" -eq 0 ]; then v="FAIL(vacuous)"
  elif [ "$f" = "0" ] && [ "$cf" = "0" ]; then v=PASS
  else v=FAIL; fi
  echo "[$label] :: $v  ${why:+<$why>}"
}

for arm in "NEW/jit:/tmp/cratonvm-zj2:" "NEW/nojit:/tmp/cratonvm-zj2:--nojit" \
           "OLD/jit:/tmp/cratonvm-old:" "OLD/nojit:/tmp/cratonvm-old:--nojit"; do
  lbl="${arm%%:*}"; rest="${arm#*:}"; bin="${rest%%:*}"; extra="${rest#*:}"
  echo "--- $lbl (10 reps) ---"
  for i in $(seq 1 10); do one "$lbl rep$i" "$bin" "$extra"; done
done
echo; echo "=== DONE ==="; date -u
