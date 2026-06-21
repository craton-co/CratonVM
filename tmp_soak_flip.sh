#!/usr/bin/env bash
# inc 29 flip soak: CRATONVM_JIT_IR_LONG + CRATONVM_JIT_IR_CALL_SPECIAL.
# The binary is FLIPPED (both default-ON), so:
#   ON  = default (no env)         OFF = CRATONVM_JIT_IR_LONG=0 CRATONVM_JIT_IR_CALL_SPECIAL=0
# Require ON == OFF for every workload, and == HotSpot where a golden exists.
set -u
cd /c/craton/CratonVM
BIN="${BIN:-/c/craton/CratonVM-flip/target/release/cratonvm.exe}"
JH="C:/Program Files/Java/jdk-25"
CO=(--java-home "$JH" --stack-dump-on-timeout 0)
OFFENV=(CRATONVM_JIT_IR_LONG=0 CRATONVM_JIT_IR_CALL_SPECIAL=0)
norm(){ sed -E 's/[0-9]+ *ms/<MS> ms/g; s/Time:[[:space:]]*[0-9]+/Time: <MS>/g; s/_ms=[0-9]+/_ms=<MS>/g; s/elapsed[ =:][0-9]+/elapsed <MS>/g'; }
pass=0; fail=0
ok(){ echo "  PASS: $1"; pass=$((pass+1)); }
bad(){ echo "  FAIL: $1"; fail=$((fail+1)); }
echo "#### binary"; ls -la "$BIN" || exit 2

echo "#### 1. bt checksums (ON==OFF==HotSpot; long-heavy)"
declare -A BT=( [10]=135854 [14]=3222190 [16]=14985902 [18]=68332206 )
for d in 10 14 16 18; do
  xmx=2g; [ "$d" -ge 16 ] && xmx=8g
  on=$("$BIN" "${CO[@]}" --Xmx $xmx -cp bench BenchSuite bintrees$d 2>&1 | grep -oE 'checksum=[0-9]+'|head -1); on=${on#checksum=}
  off=$(env "${OFFENV[@]}" "$BIN" "${CO[@]}" --Xmx $xmx -cp bench BenchSuite bintrees$d 2>&1 | grep -oE 'checksum=[0-9]+'|head -1); off=${off#checksum=}
  echo "bt$d: ON=$on OFF=$off want=${BT[$d]}"
  [ "$on" = "$off" ] && [ "$on" = "${BT[$d]}" ] && ok "bt$d" || bad "bt$d"
done

echo "#### 2. long + invokespecial probes (ON==OFF==HotSpot)"
declare -A PROBE=(
  ["irlong:IrLong"]=3588644437398634000 ["irlong:IrLong2"]=4898113815606063616
  ["irlong:IrLong3"]=540002100000 ["irlong:IrLong4"]=2336681890816
  ["irspecial:IrSpecial"]=1442980800000 ["ircall:IrCall"]=23762906400000
)
for key in "${!PROBE[@]}"; do
  dir=${key%%:*}; cls=${key##*:}; want=${PROBE[$key]}
  on=$("$BIN" "${CO[@]}" --Xmx 2g -cp scratch/$dir $cls 2>&1 | grep -oE 'total=[0-9-]+'|head -1); on=${on#total=}
  off=$(env "${OFFENV[@]}" "$BIN" "${CO[@]}" --Xmx 2g -cp scratch/$dir $cls 2>&1 | grep -oE 'total=[0-9-]+'|head -1); off=${off#total=}
  echo "$cls: ON=$on OFF=$off want=$want"
  [ "$on" = "$off" ] && [ "$on" = "$want" ] && ok "$cls" || bad "$cls"
done

echo "#### 3. broad bench differential (ON==OFF==HotSpot, timing masked)"
BENCH="binarytrees fannkuch IntegrationTest GenPair FieldCheck NBody3D NBodyMini Benchmark QuickBench MatrixJIT MatrixScale IntrinsicBench FullStackBench TestLambda TestStream TestSwitch TestEnum TestGenerics TestSort TestInterface TestVarargs TestPair TestEnum SieveBench"
for c in $BENCH; do
  [ -f "bench/$c.class" ] || continue
  hs=$(java -cp bench "$c" 2>&1 | norm)
  on=$(timeout 120 "$BIN" "${CO[@]}" --Xmx 2g -cp bench "$c" 2>&1 | norm)
  off=$(timeout 120 env "${OFFENV[@]}" "$BIN" "${CO[@]}" --Xmx 2g -cp bench "$c" 2>&1 | norm)
  if [ "$on" = "$off" ] && [ "$on" = "$hs" ]; then ok "$c"; else
    if [ "$on" != "$off" ]; then bad "$c (ON != OFF — flip changed behavior)"; diff <(echo "$off") <(echo "$on")|head -6;
    else bad "$c (ON==OFF but != HotSpot — pre-existing)"; fi
  fi
done

echo "#### 4. QuickBenchLong (long-heavy real-ish workload) ON==OFF"
on=$(timeout 300 "$BIN" "${CO[@]}" --Xmx 4g -cp bench QuickBenchLong 2>&1 | norm)
off=$(timeout 300 env "${OFFENV[@]}" "$BIN" "${CO[@]}" --Xmx 4g -cp bench QuickBenchLong 2>&1 | norm)
if [ -n "$on" ] && [ "$on" = "$off" ]; then ok "QuickBenchLong (ON==OFF, $(echo "$on"|wc -l) lines)"; else bad "QuickBenchLong (ON!=OFF or empty)"; diff <(echo "$off") <(echo "$on")|head -8; fi

echo "#### SUMMARY pass=$pass fail=$fail"
