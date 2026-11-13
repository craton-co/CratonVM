/*
 * j27 wave 8, lane W8DSP: the HotSpot-only stability measurement of page
 * docs/feature-designs/jep358-helpful-npe.md
 * (the round-21 probe R21NpecompatRows was lost with C:\craton\jr21-ssh; this
 * one rebuilds its `ao-mixed-*` rows). One `fact` line per class: the
 * getDeclaredMethods() names in the order the VM answers them. Nothing else is
 * printed; the lines are deterministic per VM only if the VM's order is.
 *
 * The question is about HOTSPOT, not CratonVM:
 *   javac -d /tmp/j27w8ao tools/probes/jit/j27/J27W8NpeOrderRows.java
 * (JDK 20+ already relocates the archive by default, ArchiveRelocationMode=1, so
 * the third arm maps it at the requested address instead; the flag is
 * diagnostic -- if HotSpot 25 refuses it, drop that arm and say so.)
 *   for arm in "" "-Xshare:off" "-XX:+UnlockDiagnosticVMOptions -XX:ArchiveRelocationMode=0"; do
 *     for i in 1 2 3 4 5; do java $arm -cp /tmp/j27w8ao J27W8NpeOrderRows > /tmp/ao-"${arm// /_}"-$i.txt; done
 *   done
 *   md5sum /tmp/ao-*.txt
 * Decides: if the five runs of ANY arm differ, HotSpot's order for run-time names is
 * not reproducible: retire the page as an external gap (no CratonVM change). If all
 * fifteen agree, compare with the rule "names the CDS archive holds first (in
 * archive order), then run-time names by first appearance in the class's constant
 * pool" (`javap -v` lists the pool); only an exact match justifies porting the
 * constructor path's approximation to getDeclaredMethods0 (behind a default-off
 * switch, Spring batteries first).
 * CratonVM (both modes) answers class-file order with bridge adjacency; its
 * lines must be identical across runs (a CratonVM self-check):
 *   for i in 1 2 3; do cratonvm -cp /tmp/j27w8ao J27W8NpeOrderRows | md5sum; done
 */
import java.lang.reflect.Method;
import java.util.function.Supplier;

public class J27W8NpeOrderRows {
    /** Archived names (common JDK method names) mixed with run-time ones. */
    static class Mixed {
        public String toString() { return "m"; }
        public int zqRuntimeOne() { return 1; }
        public int size() { return 0; }
        public int zqRuntimeTwo() { return 2; }
        public Object get() { return null; }
        public void run() { }
        public int zqRuntimeThree() { return 3; }
        public boolean isEmpty() { return true; }
    }

    /** Only run-time names, declared out of alphabetical order. */
    static class RunTimeOnly {
        void zqDelta() { }
        void zqAlpha() { }
        void zqCharlie() { }
        void zqBravo() { }
    }

    /** Lambda bodies (`lambda$...$N`) beside archived names. */
    static class Lambdas {
        static Supplier<String> a() { return () -> "a"; }
        static Supplier<String> b() { return () -> "b"; }
        public int hashCode() { return 7; }
        static Runnable c() { return () -> { }; }
    }

    /** A bridge (`compareTo(Object)`) beside its target. */
    static class Bridged implements Comparable<Bridged> {
        public int compareTo(Bridged o) { return 0; }
        public int zqAfterBridge() { return 0; }
        public boolean equals(Object o) { return o == this; }
    }

    public static void main(String[] args) {
        row("ao-mixed-plain", Mixed.class);
        row("ao-mixed-runtime-only", RunTimeOnly.class);
        row("ao-mixed-lambdas", Lambdas.class);
        row("ao-mixed-bridge", Bridged.class);
    }

    static void row(String key, Class<?> c) {
        StringBuilder sb = new StringBuilder();
        for (Method m : c.getDeclaredMethods()) {
            if (sb.length() > 0) {
                sb.append(',');
            }
            sb.append(m.getName());
            if (m.isBridge()) {
                sb.append("(bridge)");
            }
        }
        System.out.println("fact " + key + " " + sb);
    }
}
