#!/usr/bin/env bash
# hibfix-arms.sh <binary> <rounds> — N arms, INTERLEAVED round-robin.
#
# Arms are "label:ENV=V,ENV=V" or "label:--vmflag". One run of each arm per
# round, so a drifting host and a drifting failure rate hit every arm equally.
# The measure is `rows` out of 1440, which is continuous: a severe run sends
# ~130 of ~1100 inserts, so arms separate long before a pass count would.
set -uo pipefail
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
HERE="C:/craton/CratonVM/apps/hibernate-reactive-suite-runner"
BIN="$1"; ROUNDS="${2:-10}"; shift 2
ARMS=("$@")
THREADS="${DUP_THREADS:-24}"; N="${DUP_N:-60}"
EXPECT=$((THREADS * N))
cd "$HERE" || exit 1
mkdir -p runs/arms
declare -A FAIL LOST
for a in "${ARMS[@]}"; do FAIL[${a%%:*}]=0; LOST[${a%%:*}]=0; done
for r in $(seq 1 "$ROUNDS"); do
  for a in "${ARMS[@]}"; do
    label="${a%%:*}"; spec="${a#*:}"
    env_pairs=(); vmflags=()
    case "$spec" in
      --*) vmflags=("$spec") ;;
      none) ;;
      *) IFS=',' read -ra kv <<< "$spec"; for p in "${kv[@]}"; do env_pairs+=("$p"); done ;;
    esac
    tag="$label-$r"
    env "${env_pairs[@]}" ./hibfix-mtins-run.sh "$tag" "$BIN" hibfix-common-mysql-ext.args \
      "${vmflags[@]}" -Dmti.threads="$THREADS" -Dmti.n="$N" \
      -- org.hibernate.reactive.MultithreadedInsertionWithLazyConnectionTest \
      testIdentityGeneratorWithTransaction >/dev/null 2>&1
    cp "runs/mtins/$tag.log" "runs/arms/$tag.log" 2>/dev/null
    rows=$(docker exec mysql mysql -N -uhreact -phreact hreact \
      -e "select count(*) from Entity;" 2>/dev/null | tr -d '\r')
    rows=${rows:-0}
    mark=""
    if [ "$rows" != "$EXPECT" ]; then
      FAIL[$label]=$(( ${FAIL[$label]} + 1 ))
      LOST[$label]=$(( ${LOST[$label]} + EXPECT - rows ))
      mark=" LOST=$((EXPECT-rows))"
    fi
    echo "  round $r $label: rows=$rows/$EXPECT$mark"
  done
done
echo "=== TOTALS over $ROUNDS rounds ==="
for a in "${ARMS[@]}"; do l="${a%%:*}"; echo "  $l: failures=${FAIL[$l]}/$ROUNDS rows_lost=${LOST[$l]}"; done
