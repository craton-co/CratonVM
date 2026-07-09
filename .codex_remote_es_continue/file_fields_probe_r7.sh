set -euo pipefail
BIN=/data/data/bin/cratonvm-es-suite-continue-20260709-124629-r7
PROBEDIR=/tmp/craton-es-file-fields-r7
rm -rf "$PROBEDIR"
mkdir -p "$PROBEDIR"
cat > "$PROBEDIR/FileFieldsProbe.java" <<'JAVA'
import java.io.*;
import java.lang.reflect.*;
public class FileFieldsProbe {
  public static void main(String[] args) throws Exception {
    for (Field f : File.class.getDeclaredFields()) {
      if (f.getName().toLowerCase().contains("fs") || f.getType().getName().contains("FileSystem")) {
        System.out.println(f.getName() + " type=" + f.getType().getName() + " mods=" + Modifier.toString(f.getModifiers()));
      }
    }
  }
}
JAVA
javac -d "$PROBEDIR" "$PROBEDIR/FileFieldsProbe.java"
echo HOTSPOT
java -cp "$PROBEDIR" FileFieldsProbe
echo CRATON
set +e
timeout 30s "$BIN" --java-home /usr/lib/jvm/java-21-openjdk-amd64 --Xmx 512m -cp "$PROBEDIR" FileFieldsProbe >"$PROBEDIR/craton.out" 2>"$PROBEDIR/craton.err"
rc=$?
set -e
echo "rc=$rc"
sed -n '1,120p' "$PROBEDIR/craton.out"
sed -n '1,160p' "$PROBEDIR/craton.err"