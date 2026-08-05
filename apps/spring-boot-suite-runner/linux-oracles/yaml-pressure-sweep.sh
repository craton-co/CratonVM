#!/bin/bash
# Why does the pre-fix binary pass today when this failure was reproducible
# when filed? Two candidate variables, both absent on an idle host:
#   1. GC PRESSURE. The symptom is genuine corruption of a 4 MiB StringBuilder,
#      which is what a GC/JIT root or write-barrier bug looks like - and it can
#      only bite if collections actually happen. Shrinking the heap raises GC
#      frequency WITHOUT loading the shared host (deliberately loading it would
#      invalidate other sessions' runs).
#   2. CPU CONTENTION, which shifts compile/OSR timing relative to the loop.
#      taskset squeezes only this process, again leaving the host alone.
# An OOM at a small heap is an expected, uninteresting failure - the script
# records the REASON so a real corruption is never confused with running out.
exec > /tmp/pressure-results.txt 2>&1
SB=/data/data/springboot-jsonreader-deprecation-20260718
JDK=/data/jdk25-real-20260717/jdk-25.0.3+9
CP="$SB/sb-runner:$(cat $SB/core/spring-boot/build/cratonvm-test-cp.txt)"
cd "$SB/core/spring-boot" || exit 9
echo "host: $(cat /proc/loadavg) nproc=$(nproc)"

one() {  # one <label> <bin> <heap> <cpus|->
  local label="$1" bin="$2" heap="$3" cpus="$4" out rc res f cf t why v pre=()
  [ "$cpus" != "-" ] && pre=(taskset -c "$cpus")
  out=$("${pre[@]}" timeout 900 "$bin" --java-home "$JDK" --Xmx "$heap" \
          -Dfile.encoding=UTF-8 -cp "$CP" OneMethodRunner \
          org.springframework.boot.env.OriginTrackedYamlLoaderTests \
          canLoadFilesBiggerThan3Mb 2>&1)
  rc=$?
  res=$(echo "$out" | grep -o 'SBRUNNER_RESULT.*' | head -1)
  why=$(echo "$out" | grep -oE "OutOfMemoryError|expected .{0,25}|line [0-9]+, column [0-9]+" | head -1)
  f=$(echo "$res"  | grep -oE '(^| )failed=[0-9]+'      | grep -oE '[0-9]+')
  cf=$(echo "$res" | grep -oE 'containersFailed=[0-9]+' | grep -oE '[0-9]+')
  t=$(echo "$res"  | grep -oE 'tests=[0-9]+'            | grep -oE '[0-9]+')
  if   [ "$rc" = "124" ];      then v=TIMEOUT
  elif [ -z "$res" ];          then v="NORESULT rc=$rc"
  elif [ "${t:-0}" -eq 0 ];    then v="FAIL(vacuous)"
  elif [ "$f" = "0" ] && [ "$cf" = "0" ]; then v=PASS
  else v=FAIL; fi
  echo "[$label heap=$heap cpus=$cpus] :: $v  ${why:+<$why>}"
}

echo; echo "########## GC PRESSURE (pre-fix binary) ##########"
for h in 2g 1g 512m 384m 256m; do for i in 1 2; do one "OLD" /tmp/cratonvm-old "$h" -; done; done

echo; echo "########## CPU CONTENTION (pre-fix binary) ##########"
for c in 0 0-1 0-3; do for i in 1 2; do one "OLD" /tmp/cratonvm-old 2g "$c"; done; done

echo; echo "########## whichever arm went red, repeat on CURRENT dev ##########"
for h in 512m 384m 256m; do one "NEW" /tmp/cratonvm-zj2 "$h" -; done
for c in 0 0-1; do one "NEW" /tmp/cratonvm-zj2 2g "$c"; done

echo; echo "=== DONE ==="; date -u
