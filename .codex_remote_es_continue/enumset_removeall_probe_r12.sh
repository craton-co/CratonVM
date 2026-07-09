set -euo pipefail
BIN=/data/data/bin/cratonvm-es-suite-continue-20260709-124629-r12
PROBEDIR=/tmp/craton-es-enumset-removeall-r12
rm -rf "$PROBEDIR"
mkdir -p "$PROBEDIR"
cat > "$PROBEDIR/EnumSetRemoveAllProbe.java" <<'JAVA'
import java.time.temporal.ChronoField;
import java.util.*;
public class EnumSetRemoveAllProbe {
  static void dump(String label, Set<ChronoField> s) {
    System.out.println(label + " class=" + s.getClass().getName() + " size=" + s.size() + " it=" + s.iterator());
  }
  public static void main(String[] args) {
    Set<ChronoField> mandatory = Set.of(ChronoField.YEAR, ChronoField.MONTH_OF_YEAR, ChronoField.DAY_OF_MONTH);
    Set<ChronoField> allowed = Set.of(ChronoField.YEAR, ChronoField.MONTH_OF_YEAR, ChronoField.DAY_OF_MONTH, ChronoField.HOUR_OF_DAY);
    EnumSet<ChronoField> copy = EnumSet.copyOf(mandatory);
    dump("mandatory", mandatory);
    dump("allowed", allowed);
    dump("copy", copy);
    System.out.println("removeAll=" + copy.removeAll(allowed));
    dump("after", copy);
  }
}
JAVA
javac -d "$PROBEDIR" "$PROBEDIR/EnumSetRemoveAllProbe.java"
echo HOTSPOT
java -cp "$PROBEDIR" EnumSetRemoveAllProbe
echo CRATON
set +e
timeout 30s "$BIN" --java-home /usr/lib/jvm/java-21-openjdk-amd64 --Xmx 512m -cp "$PROBEDIR" EnumSetRemoveAllProbe >"$PROBEDIR/craton.out" 2>"$PROBEDIR/craton.err"
rc=$?
set -e
echo "rc=$rc"
sed -n '1,180p' "$PROBEDIR/craton.out"
sed -n '1,220p' "$PROBEDIR/craton.err"