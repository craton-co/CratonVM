set -euo pipefail
ROOT=/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun
ES="$ROOT/apps/elasticsearch"
BIN=/data/data/bin/cratonvm-es-suite-continue-20260709-124629-r11
CP=$(tr -d '\r' < "$ES/libs/core/build/craton-testcp.txt" | paste -sd: -)
PROBEDIR=/tmp/craton-es-indexversions-probe-r11
rm -rf "$PROBEDIR"
mkdir -p "$PROBEDIR"
cat > "$PROBEDIR/IndexVersionsResourceProbe.java" <<'JAVA'
import java.io.*;
import java.nio.charset.StandardCharsets;
public class IndexVersionsResourceProbe {
  public static void main(String[] args) throws Exception {
    String name = "/org/elasticsearch/index/IndexVersions.csv";
    try (InputStream in = IndexVersionsResourceProbe.class.getResourceAsStream(name)) {
      System.out.println("stream=" + in);
      if (in == null) return;
      byte[] first = in.readNBytes(80);
      System.out.println("firstBytes=" + first.length + " text=" + new String(first, StandardCharsets.ISO_8859_1).replace("\r", "<CR>").replace("\n", "<LF>"));
    }
    try (InputStream in = IndexVersionsResourceProbe.class.getResourceAsStream(name);
         BufferedReader br = new BufferedReader(new InputStreamReader(in, StandardCharsets.UTF_8))) {
      for (int i = 0; i < 8; i++) {
        String line = br.readLine();
        System.out.println("line" + i + " len=" + (line == null ? -1 : line.length()) + " [" + line + "]");
      }
    }
  }
}
JAVA
javac -cp "$CP" -d "$PROBEDIR" "$PROBEDIR/IndexVersionsResourceProbe.java"
echo HOTSPOT
java -cp "$PROBEDIR:$CP" IndexVersionsResourceProbe
echo CRATON
set +e
timeout 30s "$BIN" --java-home /usr/lib/jvm/java-21-openjdk-amd64 --Xmx 512m -cp "$PROBEDIR:$CP" IndexVersionsResourceProbe >"$PROBEDIR/craton.out" 2>"$PROBEDIR/craton.err"
rc=$?
set -e
echo "rc=$rc"
sed -n '1,180p' "$PROBEDIR/craton.out"
sed -n '1,180p' "$PROBEDIR/craton.err"