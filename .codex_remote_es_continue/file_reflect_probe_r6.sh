set -euo pipefail
BIN=/data/data/bin/cratonvm-es-suite-continue-20260709-124629-r6
PROBEDIR=/tmp/craton-es-file-reflect-r6
rm -rf "$PROBEDIR"
mkdir -p "$PROBEDIR"
cat > "$PROBEDIR/FileReflectProbe.java" <<'JAVA'
import java.io.*;
import java.lang.reflect.*;
public class FileReflectProbe {
  public static void main(String[] args) throws Exception {
    Class<?> file = File.class;
    for (Field f : file.getDeclaredFields()) {
      if (f.getType().getName().equals("java.io.FileSystem") || f.getName().equalsIgnoreCase("fs")) {
        f.setAccessible(true);
        Object v = f.get(null);
        System.out.println(f.getName() + " " + f.getType().getName() + " = " + v);
      }
    }
  }
}
JAVA
javac -d "$PROBEDIR" "$PROBEDIR/FileReflectProbe.java"
echo HOTSPOT
java -cp "$PROBEDIR" FileReflectProbe
echo CRATON
set +e
timeout 30s "$BIN" --java-home /usr/lib/jvm/java-21-openjdk-amd64 --Xmx 512m -cp "$PROBEDIR" FileReflectProbe >"$PROBEDIR/craton.out" 2>"$PROBEDIR/craton.err"
rc=$?
set -e
echo "rc=$rc"
sed -n '1,120p' "$PROBEDIR/craton.out"
sed -n '1,160p' "$PROBEDIR/craton.err"