set -euo pipefail
BIN=/data/data/bin/cratonvm-es-suite-continue-20260709-124629-r8
PROBEDIR=/tmp/craton-es-filefs-probe-r8
rm -rf "$PROBEDIR"
mkdir -p "$PROBEDIR"
cat > "$PROBEDIR/FileFsProbe.java" <<'JAVA'
import java.io.*;
import java.util.*;
public class FileFsProbe {
  public static void main(String[] args) throws Exception {
    File f = new File(System.getProperty("java.home"), "lib/tzdb.dat");
    System.out.println("file=" + f.getPath() + " exists=" + f.exists() + " isFile=" + f.isFile());
    try (FileInputStream in = new FileInputStream(f)) {
      System.out.println("first=" + in.read());
    }
    System.out.println("zone=" + TimeZone.getDefault().toZoneId());
  }
}
JAVA
javac -d "$PROBEDIR" "$PROBEDIR/FileFsProbe.java"
set +e
timeout 30s "$BIN" --java-home /usr/lib/jvm/java-21-openjdk-amd64 --Xmx 512m -cp "$PROBEDIR" FileFsProbe >"$PROBEDIR/craton.out" 2>"$PROBEDIR/craton.err"
rc=$?
set -e
echo "rc=$rc"
sed -n '1,160p' "$PROBEDIR/craton.out"
sed -n '1,160p' "$PROBEDIR/craton.err"