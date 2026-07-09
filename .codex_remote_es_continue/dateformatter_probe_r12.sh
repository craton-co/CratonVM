set -euo pipefail
ROOT=/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun
ES="$ROOT/apps/elasticsearch"
JDK=/usr/lib/jvm/java-21-openjdk-amd64
BIN=/data/data/bin/cratonvm-es-suite-continue-20260709-124629-r12
CP=$(tr -d '\r' < "$ES/libs/core/build/craton-testcp.txt" | paste -sd: -)
PROBEDIR=/tmp/craton-es-dateformatter-probe-r12
rm -rf "$PROBEDIR"
mkdir -p "$PROBEDIR"
cat > "$PROBEDIR/DateFormatterProbe.java" <<'JAVA'
public class DateFormatterProbe {
  public static void main(String[] args) throws Exception {
    Class<?> c = Class.forName("org.elasticsearch.common.time.DateFormatter");
    Object f = c.getMethod("forPattern", String.class).invoke(null, args.length == 0 ? "strict_date_optional_time" : args[0]);
    System.out.println("formatter=" + f);
  }
}
JAVA
javac -d "$PROBEDIR" "$PROBEDIR/DateFormatterProbe.java"
echo HOTSPOT
"$JDK/bin/java" -cp "$PROBEDIR:$CP" DateFormatterProbe | head -n 20
echo CRATON
set +e
timeout 30s "$BIN" --java-home "$JDK" --Xmx 512m -cp "$PROBEDIR:$CP" DateFormatterProbe >"$PROBEDIR/craton.out" 2>"$PROBEDIR/craton.err"
rc=$?
set -e
echo "rc=$rc"
sed -n '1,120p' "$PROBEDIR/craton.out"
sed -n '1,240p' "$PROBEDIR/craton.err"