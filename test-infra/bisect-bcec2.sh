#!/usr/bin/env bash
# bc-math-ec bisection — generate skip lists for specific hot methods.
# Each entry is a Class.method pair (slash-class, dot-method).

set -u
JUNIT='C:\Users\Victor\AppData\Local\Temp\junit-3.8.2.jar'
BC_CP='core/build/classes/java/main;core/build/classes/java/test;core/build/resources/main;core/build/resources/test'

cd C:/craton/CratonVM/apps/_test-suites/bc-java

# Build comma-separated skip list from class lists.
skip_methods() {
  local pkg="$1"; shift
  local methods="$@"
  local list=""
  for m in $methods; do
    [ -z "$list" ] || list+=","
    list+="$pkg.$m"
  done
  echo "$list"
}

run_skip() {
  local label="$1"
  local skip="$2"
  CRATONVM_JIT_BISECT_SKIP="$skip" timeout 60 \
    C:/craton/CratonVM/target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
    --stack-dump-on-timeout 0 -Xmx1g \
    -cp "${BC_CP};${JUNIT}" \
    junit.textui.TestRunner org.bouncycastle.math.ec.test.AllTests > /tmp/bisect.log 2>&1
  local rc=$?
  local segv=$(grep -c "inconsistent header\|Segmentation" /tmp/bisect.log)
  echo "$label: rc=$rc segv=$segv"
}

# Try each likely culprit class -- skip ALL its public/static methods
# by listing common JIT-hot ones.

# Mod (modular arithmetic — hot in EC) — already tried but with limited methods
mod_skip=$(skip_methods 'org/bouncycastle/math/raw/Mod' \
  modOddInverse modOddInverseVar checkedModOddInverse checkedModOddInverseVar \
  inverse32 divsteps32 divsteps62 gcd modAdd modSubtract subtractFrom add invert)
run_skip "Mod-all-methods" "$mod_skip"

# Nat -- generic operations
nat_skip=$(skip_methods 'org/bouncycastle/math/raw/Nat' \
  add cadd csub sub mul square shiftDownBit shiftUpBit fromBigInteger \
  iszero isOne lessThan equal shiftDown getBit shiftDownWord)
run_skip "Nat-all" "$nat_skip"

# Nat256 -- P-256-sized
nat256_skip=$(skip_methods 'org/bouncycastle/math/raw/Nat256' \
  add cadd csub sub mul square fromBigInteger toBigInteger create \
  iszero isOne lessThan equal mulAddTo squareToExt mulToExt copy)
run_skip "Nat256-all" "$nat256_skip"

# Nat192 -- P-192-sized
nat192_skip=$(skip_methods 'org/bouncycastle/math/raw/Nat192' \
  add cadd csub sub mul square fromBigInteger toBigInteger create \
  iszero isOne lessThan equal mulAddTo squareToExt mulToExt copy)
run_skip "Nat192-all" "$nat192_skip"

# ECPoint.Fp variants
ecpoint_skip=$(skip_methods 'org/bouncycastle/math/ec/ECPoint' \
  add twice multiply normalize equals getEncoded getAffineXCoord getAffineYCoord \
  isInfinity)
run_skip "ECPoint-all" "$ecpoint_skip"

# ECFieldElement.Fp variants
ecfe_skip=$(skip_methods 'org/bouncycastle/math/ec/ECFieldElement$Fp' \
  add subtract multiply divide square negate)
run_skip "ECFieldElement.Fp-all" "$ecfe_skip"

# AbstractECMultiplier
em_skip=$(skip_methods 'org/bouncycastle/math/ec/AbstractECMultiplier' \
  multiply multiplyPositive)
run_skip "AbstractECMultiplier-all" "$em_skip"

# ECAlgorithms
eca_skip=$(skip_methods 'org/bouncycastle/math/ec/ECAlgorithms' \
  shamirsTrick sumOfMultiplies sumOfTwoMultiplies referenceMultiply)
run_skip "ECAlgorithms-all" "$eca_skip"

# WNafUtil
wnaf_skip=$(skip_methods 'org/bouncycastle/math/ec/WNafUtil' \
  generateWindowNaf generateNaf precompute mapPointWithPrecomp)
run_skip "WNafUtil-all" "$wnaf_skip"

# GLVMultiplier
glv_skip=$(skip_methods 'org/bouncycastle/math/ec/GLVMultiplier' \
  multiplyPositive)
run_skip "GLVMultiplier-all" "$glv_skip"
