set -euo pipefail
BIN=/data/data/bin/cratonvm-es-suite-continue-20260709-124629-r9
PROBEDIR=/tmp/craton-es-thread-unsafe-r9
rm -rf "$PROBEDIR"
mkdir -p "$PROBEDIR"
cat > "$PROBEDIR/ThreadUnsafeProbe.java" <<'JAVA'
import jdk.internal.misc.Unsafe;
public class ThreadUnsafeProbe {
  public static void main(String[] args) throws Exception {
    Unsafe u = Unsafe.getUnsafe();
    Thread t = Thread.currentThread();
    for (String name : new String[]{"name", "group", "holder", "contextClassLoader"}) {
      try {
        long off = u.objectFieldOffset(Thread.class, name);
        Object v = u.getReference(t, off);
        System.out.println(name + " off=" + off + " value=" + v + " class=" + (v == null ? "null" : v.getClass().getName()));
      } catch (Throwable e) {
        System.out.println(name + " err=" + e);
      }
    }
    System.out.println("getThreadGroup=" + t.getThreadGroup());
  }
}
JAVA
javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d "$PROBEDIR" "$PROBEDIR/ThreadUnsafeProbe.java"
echo HOTSPOT
java --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp "$PROBEDIR" ThreadUnsafeProbe
echo CRATON
set +e
timeout 30s "$BIN" --java-home /usr/lib/jvm/java-21-openjdk-amd64 --Xmx 512m --add-exports=java.base/jdk.internal.misc=ALL-UNNAMED -cp "$PROBEDIR" ThreadUnsafeProbe >"$PROBEDIR/craton.out" 2>"$PROBEDIR/craton.err"
rc=$?
set -e
echo "rc=$rc"
sed -n '1,180p' "$PROBEDIR/craton.out"
sed -n '1,180p' "$PROBEDIR/craton.err"