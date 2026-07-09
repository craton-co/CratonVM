set -euo pipefail
BIN=/data/data/bin/cratonvm-es-suite-continue-20260709-124629-r9
PROBEDIR=/tmp/craton-es-threadgroup-unsafe-r9
rm -rf "$PROBEDIR"
mkdir -p "$PROBEDIR"
cat > "$PROBEDIR/ThreadGroupUnsafeProbe.java" <<'JAVA'
import jdk.internal.misc.Unsafe;
public class ThreadGroupUnsafeProbe {
  public static void main(String[] args) throws Exception {
    Unsafe u = Unsafe.getUnsafe();
    ThreadGroup g = Thread.currentThread().getThreadGroup();
    System.out.println("group=" + g + " class=" + g.getClass().getName());
    for (String name : new String[]{"name", "parent", "maxPriority", "destroyed"}) {
      try {
        long off = u.objectFieldOffset(ThreadGroup.class, name);
        Object v = null;
        try { v = u.getReference(g, off); } catch (Throwable e) { System.out.println(name + " getRefErr=" + e); }
        System.out.println(name + " off=" + off + " ref=" + v + " class=" + (v == null ? "null" : v.getClass().getName()));
      } catch (Throwable e) {
        System.out.println(name + " err=" + e);
      }
    }
  }
}
JAVA
javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d "$PROBEDIR" "$PROBEDIR/ThreadGroupUnsafeProbe.java"
echo HOTSPOT
java --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp "$PROBEDIR" ThreadGroupUnsafeProbe
echo CRATON
set +e
timeout 30s "$BIN" --java-home /usr/lib/jvm/java-21-openjdk-amd64 --Xmx 512m --add-exports=java.base/jdk.internal.misc=ALL-UNNAMED -cp "$PROBEDIR" ThreadGroupUnsafeProbe >"$PROBEDIR/craton.out" 2>"$PROBEDIR/craton.err"
rc=$?
set -e
echo "rc=$rc"
sed -n '1,180p' "$PROBEDIR/craton.out"
sed -n '1,180p' "$PROBEDIR/craton.err"