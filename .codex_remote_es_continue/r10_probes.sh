set -euo pipefail
BIN=/data/data/bin/cratonvm-es-suite-continue-20260709-124629-r10
PROBEDIR=/tmp/craton-es-r10-probes
rm -rf "$PROBEDIR"
mkdir -p "$PROBEDIR"
cat > "$PROBEDIR/ThreadGroupUnsafeProbe.java" <<'JAVA'
import jdk.internal.misc.Unsafe;
public class ThreadGroupUnsafeProbe {
  public static void main(String[] args) throws Exception {
    Unsafe u = Unsafe.getUnsafe();
    ThreadGroup g = Thread.currentThread().getThreadGroup();
    for (String name : new String[]{"name", "parent"}) {
      long off = u.objectFieldOffset(ThreadGroup.class, name);
      Object v = u.getReference(g, off);
      System.out.println(name + " off=" + off + " ref=" + v + " class=" + (v == null ? "null" : v.getClass().getName()));
    }
  }
}
JAVA
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
javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d "$PROBEDIR" "$PROBEDIR/ThreadGroupUnsafeProbe.java"
javac -d "$PROBEDIR" "$PROBEDIR/FileFsProbe.java"
for cls in ThreadGroupUnsafeProbe FileFsProbe; do
  echo "CRATON $cls"
  set +e
  timeout 30s "$BIN" --java-home /usr/lib/jvm/java-21-openjdk-amd64 --Xmx 512m --add-exports=java.base/jdk.internal.misc=ALL-UNNAMED -cp "$PROBEDIR" "$cls" >"$PROBEDIR/$cls.out" 2>"$PROBEDIR/$cls.err"
  rc=$?
  set -e
  echo "rc=$rc"
  sed -n '1,160p' "$PROBEDIR/$cls.out"
  sed -n '1,160p' "$PROBEDIR/$cls.err"
done