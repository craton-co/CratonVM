set -euo pipefail
ROOT=/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun
ES="$ROOT/apps/elasticsearch"
BIN=/data/data/bin/cratonvm-es-suite-continue-20260709-124629-r5
CP=$(tr -d '\r' < "$ES/libs/core/build/craton-testcp.txt" | paste -sd: -)
PROBEDIR=/tmp/craton-es-tokenize-probe-r5
rm -rf "$PROBEDIR"
mkdir -p "$PROBEDIR"
cat > "$PROBEDIR/TokenizeProbe.java" <<'JAVA'
import org.apache.logging.log4j.util.PropertySource;
public class TokenizeProbe {
  public static void main(String[] args) {
    System.out.println(PropertySource.Util.tokenize(args.length == 0 ? "log4j2.StatusLogger.level" : args[0]));
  }
}
JAVA
cat > "$PROBEDIR/RegexProbe.java" <<'JAVA'
import java.util.regex.*;
public class RegexProbe {
  public static void main(String[] args) {
    String value = args.length == 0 ? "log4j2.StatusLogger.level" : args[0];
    Pattern prefix = Pattern.compile("(^log4j2?[-._/]?|^org\\.apache\\.logging\\.log4j\\.)|(?=AsyncLogger(Config)?\\.)");
    Pattern tokenizer = Pattern.compile("([A-Z]*[a-z0-9]+|[A-Z0-9]+)[-._/]?");
    int start = 0;
    int guard = 0;
    Matcher pm = prefix.matcher(value);
    while (pm.find(start)) {
      System.out.println("prefix " + pm.start() + "-" + pm.end() + " group0=" + pm.group());
      start = pm.end();
      if (++guard > 10) throw new RuntimeException("prefix guard");
    }
    start = 0;
    guard = 0;
    Matcher tm = tokenizer.matcher(value);
    while (tm.find(start)) {
      System.out.println("token " + tm.start() + "-" + tm.end() + " g1=" + tm.group(1) + " g1pos=" + tm.start(1) + "-" + tm.end(1));
      start = tm.end();
      if (++guard > 20) throw new RuntimeException("token guard");
    }
  }
}
JAVA
javac -cp "$CP" -d "$PROBEDIR" "$PROBEDIR/TokenizeProbe.java" "$PROBEDIR/RegexProbe.java"
for cls in TokenizeProbe RegexProbe; do
  echo "CRATON $cls"
  set +e
  timeout 30s "$BIN" --java-home /usr/lib/jvm/java-21-openjdk-amd64 --Xmx 512m -cp "$PROBEDIR:$CP" "$cls" >"$PROBEDIR/$cls.craton.out" 2>"$PROBEDIR/$cls.craton.err"
  rc=$?
  set -e
  echo "rc=$rc"
  sed -n '1,120p' "$PROBEDIR/$cls.craton.out"
  sed -n '1,120p' "$PROBEDIR/$cls.craton.err"
done