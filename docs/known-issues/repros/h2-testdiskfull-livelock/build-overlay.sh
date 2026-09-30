#!/bin/bash
# Build the TestDiskFull diagnosis overlay. See README.md.
#
#   H2          path to the built apps/h2database/h2 checkout
#   JAVA_HOME_25 JDK 25 to compile with (also the --java-home CratonVM runs against)
#   OV          overlay output dir (default /tmp/dfull-ov)
set -eu
: "${H2:?set H2 to the built apps/h2database/h2 checkout}"
: "${JAVA_HOME_25:?set JAVA_HOME_25 to a JDK 25}"
OV="${OV:-/tmp/dfull-ov}"
HERE="$(cd "$(dirname "$0")" && pwd)"

mkdir -p "$OV/src/org/h2/test/synth" "$OV/src/org/h2/mvstore/tx" "$OV/out"
cp "$HERE/TestDiskFull.java" "$OV/src/org/h2/test/synth/TestDiskFull.java"

H2_SRC="$H2/src/main/org/h2/mvstore/tx"
python3 "$HERE/patch-transaction.py"      "$H2_SRC/Transaction.java"      "$OV/src/org/h2/mvstore/tx/Transaction.java"
python3 "$HERE/patch-transactionstore.py" "$H2_SRC/TransactionStore.java" "$OV/src/org/h2/mvstore/tx/TransactionStore.java"

CP="$H2/target/classes:$H2/target/test-classes:$(cat "$H2/craton-testcp.txt")"
"$JAVA_HOME_25/bin/javac" -nowarn -cp "$CP" -d "$OV/out" \
    "$OV/src/org/h2/test/synth/TestDiskFull.java" \
    "$OV/src/org/h2/mvstore/tx/Transaction.java" \
    "$OV/src/org/h2/mvstore/tx/TransactionStore.java"
echo "overlay built in $OV/out"
