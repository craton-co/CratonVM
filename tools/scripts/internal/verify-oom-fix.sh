#!/usr/bin/env bash
# Verify the over-large-array OOM fix: catchable OutOfMemoryError instead of
# VM abort, no regression on normal ArrayList usage, and ArrayConstructorTests
# flips ABEND -> pass under CratonVM.
set -u
export MSYS2_ARG_CONV_EXCL='*'; export MSYS_NO_PATHCONV=1
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
VM="${VM:-/c/craton/CratonVM-oomfix/target/release/cratonvm.exe}"
PROBE=/c/craton/CratonVM/spring-suite/probe
H=$(cygpath -m /c/craton/CratonVM-springrun/spring-suite)   # KRun.class lives here
SE=/c/craton/cratonvm/apps/spring-framework/spring-expression
CP="$H;$(tr -d '\r' < "$SE/build/cratonvm-testcp.txt")"

echo "############ VM = $VM"
"$VM" --version 2>&1 | head -1

echo; echo "===== (1) OomProbe: huge ArrayList/int[]/Object[] => catchable OOME, no abort ====="
"$VM" --java-home "$JDK" -cp "$PROBE" OomProbe 2>&1 | grep -vE 'watchdog armed'

echo; echo "===== (2) negative capacity => IllegalArgumentException; normal caps work ====="
cat > "$PROBE/Cap.java" <<'EOF'
import java.util.ArrayList;
public class Cap {
  public static void main(String[] a) {
    try { new ArrayList<>(-1); System.out.println("neg: NO THROW (BUG)"); }
    catch (Throwable t){ System.out.println("neg: "+t.getClass().getSimpleName()+": "+t.getMessage()); }
    ArrayList<Integer> l = new ArrayList<>(1000);
    for (int i=0;i<5000;i++) l.add(i);
    System.out.println("normal(1000)+5000adds: size="+l.size()+" first="+l.get(0)+" last="+l.get(4999));
    ArrayList<Integer> z = new ArrayList<>(0);
    z.add(7); System.out.println("zero-cap: size="+z.size()+" v="+z.get(0));
    System.out.println("DONE-CAP");
  }
}
EOF
"$JDK\bin\javac.exe" -d "$PROBE" "$PROBE/Cap.java" 2>&1 | head
"$VM" --java-home "$JDK" -cp "$PROBE" Cap 2>&1 | grep -vE 'watchdog armed'

echo; echo "===== (3) ArrayConstructorTests under CratonVM (was ABEND rc=127) ====="
"$VM" --java-home "$JDK" --stack-dump-on-timeout 0 -cp "$CP" KRun org.springframework.expression.spel.ArrayConstructorTests 2>&1 | grep -E '^(BEGIN|RESULT|FAILCAUSE|LOADERR|FATAL)'; echo "rc=$?"

echo; echo "===== (4) family probe: do other (int)-capacity collections abort? ====="
cat > "$PROBE/Fam.java" <<'EOF'
public class Fam {
  static void t(String n, Runnable r){ try { r.run(); System.out.println(n+": NO THROW"); } catch (Throwable e){ System.out.println(n+": "+e.getClass().getSimpleName()+": "+e.getMessage()); } }
  public static void main(String[] a){
    int M = Integer.MAX_VALUE;
    t("HashMap(int)", () -> { new java.util.HashMap<>(M); });
    t("HashSet(int)", () -> { new java.util.HashSet<>(M); });
    t("LinkedHashMap(int)", () -> { new java.util.LinkedHashMap<>(M); });
    t("ArrayDeque(int)", () -> { new java.util.ArrayDeque<>(M); });
    t("PriorityQueue(int)", () -> { new java.util.PriorityQueue<>(M); });
    t("Vector(int)", () -> { new java.util.Vector<>(M); });
    t("StringBuilder(int)", () -> { new StringBuilder(M); });
    System.out.println("DONE-FAM");
  }
}
EOF
"$JDK\bin\javac.exe" -d "$PROBE" "$PROBE/Fam.java" 2>&1 | head
echo "--- CratonVM ---"; "$VM" --java-home "$JDK" -cp "$PROBE" Fam 2>&1 | grep -vE 'watchdog armed' | tail -12
echo "--- HotSpot (reference) ---"; "$JDK\bin\java.exe" -cp "$PROBE" Fam 2>&1 | tail -9
echo "===== done ====="
