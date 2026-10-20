#!/usr/bin/env bash
# Reproduce the JpaLargeBlobTest retirement gate on a real JDK 25.
# Usage: run-jpa-blob-retirement-gate.sh BIN JDK OUT [REPEATS=3]
# OUT must be a new directory. Its fixture/target is a task-created build output.
set -euo pipefail
BIN=$(realpath "$1")
JDK=$(realpath "$2")
OUT=$(realpath -m "$3")
REPEATS=${4:-3}
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
[[ ! -e "$OUT" ]] || { echo "OUT already exists: $OUT" >&2; exit 2; }
mkdir -p "$OUT/classes" "$OUT/fixture/src/test/java/org/hibernate/orm/test/lob"
export JAVA_HOME="$JDK"
export PATH="$JDK/bin:$PATH"
unset CRATONVM_JDK_RANDOM
"$JDK/bin/java" -version >"$OUT/jdk.txt" 2>&1
sha256sum "$BIN" >"$OUT/binary.sha256"
git -C "$ROOT" rev-parse HEAD >"$OUT/source.txt"
PROBES=(BlobCalleeAcceptance RandomRetirementAcceptance RandomSpec SecureRandomSpec RandomLiveAcrossGc AtomicCorrectness BoxCorrectness)
SOURCES=()
for p in "${PROBES[@]}"; do SOURCES+=("$ROOT/tools/probes/$p.java"); done
"$JDK/bin/javac" -d "$OUT/classes" "${SOURCES[@]}" "$ROOT/apps/probes/RandomShadowSweep.java"
for p in "${PROBES[@]:1}" RandomShadowSweep; do
  timeout 120 "$JDK/bin/java" -Xmx512m -Dchurn=30000 -Drounds=3 -cp "$OUT/classes" "$p" >"$OUT/$p-hotspot.log" 2>&1
  grep -E '^CK|^[0-9]+ |^DONE' "$OUT/$p-hotspot.log" >"$OUT/$p-hotspot.rows"
  test -s "$OUT/$p-hotspot.rows"
  for mode in jdk-only compatible; do
    for policy in default shadow; do
      envargs=()
      [[ "$policy" == default ]] || envargs=(CRATONVM_JDK_RANDOM=0)
      name="$p-$mode-$policy"
      timeout 120 env "${envargs[@]}" "$BIN" --"$mode" --java-home "$JDK" --Xmx 512m -Dchurn=30000 -Drounds=3 -cp "$OUT/classes" "$p" >"$OUT/$name.log" 2>&1
      grep -E '^CK|^[0-9]+ |^DONE' "$OUT/$name.log" >"$OUT/$name.rows"
      diff -u "$OUT/$p-hotspot.rows" "$OUT/$name.rows" >"$OUT/$name.diff"
      printf 'SEMANTICS\t%s\tPASS\n' "$name" >>"$OUT/results.tsv"
    done
  done
done
for mode in jdk-only compatible; do
  for arm in primitive boxed atomic-inline atomic-static atomic-virtual atomic-nested random constructors; do
    name="callee-$mode-$arm"
    timeout 90 "$JDK/bin/java" -Darm="$arm" -Dn=1000000 -cp "$OUT/classes" BlobCalleeAcceptance >"$OUT/$name-hotspot.log" 2>&1
    timeout 90 env CRATONVM_DBG_MIC_PROF=1 CRATONVM_DBG_ATOMIC_INTRINSIC=1 "$BIN" --"$mode" --java-home "$JDK" --Xmx 1g -Darm="$arm" -Dn=1000000 -cp "$OUT/classes" BlobCalleeAcceptance >"$OUT/$name.log" 2>&1
    diff -u <(grep '^RESULT' "$OUT/$name-hotspot.log") <(grep '^RESULT' "$OUT/$name.log") >"$OUT/$name.diff"
    printf 'CALLEE\t%s\tPASS\n' "$name" >>"$OUT/results.tsv"
  done
  timeout 90 "$BIN" --"$mode" --java-home "$JDK" --Xmx 512m --dump-native-registry "$OUT/registry-$mode.json" -cp "$OUT/classes" RandomSpec >"$OUT/registry-$mode.log" 2>&1
  # The canonical minus token must restore the same shadow as the legacy =0.
  timeout 90 env CRATONVM_COMPAT=-jdk-random "$BIN" --"$mode" --java-home "$JDK" --Xmx 512m --dump-native-registry "$OUT/registry-$mode-shadow.json" -cp "$OUT/classes" RandomSpec >"$OUT/registry-$mode-shadow.log" 2>&1
  diff -u "$OUT/RandomSpec-hotspot.rows" <(grep '^CK' "$OUT/registry-$mode-shadow.log") >"$OUT/registry-$mode-shadow.diff"
  for gc in ZGC G1 Generational; do
    name="gc-$mode-$gc"
    timeout 120 "$BIN" --"$mode" --java-home "$JDK" --XX:UseGc "$gc" --Xmx 512m -Dchurn=200000 -Drounds=6 -cp "$OUT/classes" RandomLiveAcrossGc >"$OUT/$name.log" 2>&1
    timeout 120 "$JDK/bin/java" -Xmx512m -Dchurn=200000 -Drounds=6 -cp "$OUT/classes" RandomLiveAcrossGc >"$OUT/$name-hotspot.log" 2>&1
    diff -u <(grep '^CK' "$OUT/$name-hotspot.log") <(grep '^CK' "$OUT/$name.log") >"$OUT/$name.diff"
    printf 'GC\t%s\tPASS\n' "$name" >>"$OUT/results.tsv"
  done
done
python3 - "$OUT" <<'PY'
import json,pathlib,sys
out=pathlib.Path(sys.argv[1])
for mode in ("jdk-only","compatible"):
    doc=json.loads((out/f"registry-{mode}.json").read_text())
    entries=[r for r in doc["natives"] if r.get("class")=="java/util/Random" or r.get("class_name")=="java/util/Random"]
    assert not entries, entries
    print("REGISTRY", mode, "Random has no native registrations")
    shadow=json.loads((out/f"registry-{mode}-shadow.json").read_text())
    assert any(r.get("class")=="java/util/Random" for r in shadow["natives"])
    print("REGISTRY", mode, "canonical minus token restores native Random")
PY
fixture="$OUT/fixture"
for source in JpaLargeBlobTest LobEntity; do
  curl -fsSL "https://raw.githubusercontent.com/hibernate/hibernate-orm/7.4.5/hibernate-core/src/test/java/org/hibernate/orm/test/lob/$source.java" -o "$fixture/src/test/java/org/hibernate/orm/test/lob/$source.java"
done
(
  cd "$fixture/src/test/java/org/hibernate/orm/test/lob"
  printf '%s\n' '483bb4a8a41ef57f6e8a2d40cc750cb10b7cb6746ab876c484fc5f5dde7b7537  JpaLargeBlobTest.java' '6cd96e02e1c7888c2a2bf68849a7615a537319a65de980b9c1559ed1aad22dab  LobEntity.java' | sha256sum -c -
)
# Add only the document's method-level 120-second budget to the pinned fixture.
sed -i '/public void jpaBlobStream/i\        @org.junit.jupiter.api.Timeout(120)' "$fixture/src/test/java/org/hibernate/orm/test/lob/JpaLargeBlobTest.java"
# A successful test must persist all requested bytes. Check the stored SQL
# length after close(), adding no operation to the original read/write loop.
# Unmasked Random.nextInt() can return -1, so read calls are not payload bytes.
python3 - "$fixture/src/test/java/org/hibernate/orm/test/lob/JpaLargeBlobTest.java" <<'PY'
import pathlib, sys
p = pathlib.Path(sys.argv[1])
s = p.read_text()
needle = "inputStream.close();"
assert s.count(needle) == 1
s = s.replace(needle, needle + """
                long readCalls = 200L * 1024 * 1024 - inputStream.count.longValue();
                org.junit.jupiter.api.Assertions.assertTrue(readCalls >= LobEntity.BLOB_LENGTH);
                long storedBytes = scope.fromSession(session -> session.doReturningWork(connection -> {
                    try (java.sql.Statement statement = connection.createStatement();
                         java.sql.ResultSet result = statement.executeQuery(
                             "select octet_length(blob) from LobEntity")) {
                        org.junit.jupiter.api.Assertions.assertTrue(result.next());
                        long bytes = result.getLong(1);
                        org.junit.jupiter.api.Assertions.assertFalse(result.next());
                        return bytes;
                    }
                }));
                org.junit.jupiter.api.Assertions.assertEquals(
                    (long) LobEntity.BLOB_LENGTH, storedBytes);
                System.out.println("BLOB_READ_CALLS=" + readCalls);
                System.out.println("BLOB_BYTES_CONSUMED=" + storedBytes);
""")
p.write_text(s)
PY
cp "$ROOT/regression-suite/perf/jpa-blob-fixture.pom.xml" "$fixture/pom.xml"
mvn -q -f "$fixture/pom.xml" test-compile dependency:build-classpath -Dmdep.outputFile="$fixture/classpath.txt" >"$OUT/fixture-build.log" 2>&1
printf '%s\n' 'hibernate.dialect=org.hibernate.dialect.H2Dialect' 'hibernate.connection.driver_class=org.h2.Driver' 'hibernate.connection.url=jdbc:h2:mem:blobretire;DB_CLOSE_DELAY=-1' 'hibernate.connection.username=sa' 'hibernate.connection.password=' 'hibernate.connection.pool_size=5' 'hibernate.show_sql=false' >"$fixture/target/test-classes/hibernate.properties"
CP="$fixture/target/test-classes:$(cat "$fixture/classpath.txt")"
SELECT='org.hibernate.orm.test.lob.JpaLargeBlobTest#jpaBlobStream(org.hibernate.testing.orm.junit.SessionFactoryScope)'
# Fresh processes, alternating HotSpot and the two runtime policies. Optional
# CPU pinning applies equally to all three; load is recorded for every sample.
prefix=()
[[ -z "${BLOB_GATE_CPU:-}" ]] || prefix=(taskset -c "$BLOB_GATE_CPU")
for ((r=1; r<=REPEATS; r++)); do
  modes=(hotspot jdk-only compatible)
  (( r % 2 )) || modes=(compatible jdk-only hotspot)
  for mode in "${modes[@]}"; do
    name="hibernate-$r-$mode"
    cat /proc/loadavg >"$OUT/$name.load"
    if [[ "$mode" == hotspot ]]; then
      command=("$JDK/bin/java" -Xmx3g)
    else
      command=("$BIN" --"$mode" --java-home "$JDK" --Xmx 3g --stack-dump-on-timeout 180)
    fi
    timeout 210 /usr/bin/time -o "$OUT/$name.time" -f 'wall=%e cpu=%U sys=%S rss=%M status=%x' "${prefix[@]}" "${command[@]}" -Duser.language=en -Duser.country=US -Djunit.jupiter.execution.timeout.test.method.default=120s -cp "$CP" org.junit.platform.console.ConsoleLauncher execute --select-method "$SELECT" --details summary --disable-banner >"$OUT/$name.log" 2>&1
    grep -q '^BLOB_BYTES_CONSUMED=100000000$' "$OUT/$name.log"
    grep -Eq '\[ +1 tests successful' "$OUT/$name.log"
    grep -Eq '\[ +0 tests failed' "$OUT/$name.log"
    printf 'HIBERNATE\t%s\tPASS\t%s\n' "$name" "$(cat "$OUT/$name.time")" >>"$OUT/results.tsv"
    echo "$name PASS $(cat "$OUT/$name.time")"
  done
done
echo "BLOB-RETIREMENT-GATE PASS"
