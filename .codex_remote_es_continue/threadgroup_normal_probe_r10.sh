set -euo pipefail
BIN=/data/data/bin/cratonvm-es-suite-continue-20260709-124629-r10
PROBEDIR=/tmp/craton-es-threadgroup-normal-r10
rm -rf "$PROBEDIR"
mkdir -p "$PROBEDIR"
cat > "$PROBEDIR/ThreadGroupNormalProbe.java" <<'JAVA'
public class ThreadGroupNormalProbe {
  public static void main(String[] args) throws Exception {
    ThreadGroup g = Thread.currentThread().getThreadGroup();
    System.out.println("g=" + g);
    System.out.println("name=" + g.getName());
    System.out.println("parent=" + g.getParent());
    System.out.println("parentName=" + (g.getParent() == null ? null : g.getParent().getName()));
  }
}
JAVA
javac -d "$PROBEDIR" "$PROBEDIR/ThreadGroupNormalProbe.java"
set +e
timeout 30s "$BIN" --java-home /usr/lib/jvm/java-21-openjdk-amd64 --Xmx 512m -cp "$PROBEDIR" ThreadGroupNormalProbe >"$PROBEDIR/craton.out" 2>"$PROBEDIR/craton.err"
rc=$?
set -e
echo "rc=$rc"
sed -n '1,120p' "$PROBEDIR/craton.out"
sed -n '1,160p' "$PROBEDIR/craton.err"