// JIT round 21 orchestrator triage probe: rows for the opt-in switches that were never measured.
//   cratonvm -Xmx512m R21TriageRows        (default, then each switch =1 interleaved)
// Rows: short-loop-ib (short loops entered from interpreted callers: CRATONVM_JIT_OSR_EARLY_REENTRY /
// _REQUEST), long-osr (one long loop, OSR), sync-loop / sync-window (lock coarsening evidence:
// CRATONVM_JIT_IR_EVIDENCE_SIMPLIFIED_BEFORE_GROWTH, CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_WINDOW_EVIDENCE),
// class-name / class-hash (CRATONVM_CLASS_ID_FROM_MIRROR_MEMO / _TABLE). Check lines are deterministic.
import java.util.*;

public class R21TriageRows {
    static int sink;
    static final Object LOCK = new Object();
    static int counter;

    static int shortLoop(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) s += a[i] * 3 + 1;
        return s;
    }

    static long longOsr(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) { s += (i ^ (i >>> 3)) & 0xff; }
        return s;
    }

    static int syncLoop(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) { synchronized (LOCK) { counter++; s += counter & 7; } }
        return s;
    }

    static final StringBuffer SB = new StringBuffer();
    static int syncWindow(int n) {
        SB.setLength(0);
        for (int i = 0; i < n; i++) { SB.append('a'); if (SB.length() > 64) SB.setLength(0); }
        return SB.length();
    }

    static final Class<?>[] CLASSES = { String.class, Integer.class, ArrayList.class, HashMap.class,
        R21TriageRows.class, int[].class, Object[].class, Runnable.class };

    static int className(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) s += CLASSES[i & 7].getName().length();
        return s;
    }

    static int classHash(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) s += System.identityHashCode(CLASSES[i & 7]) & 1;
        return s;
    }

    interface Row { long run(); }

    static void time(String name, int ops, Row r) {
        for (int w = 0; w < 5; w++) sink += (int) r.run();
        long[] t = new long[9];
        for (int k = 0; k < t.length; k++) {
            long t0 = System.nanoTime();
            sink += (int) r.run();
            t[k] = System.nanoTime() - t0;
        }
        Arrays.sort(t);
        System.out.println("time " + name + " " + String.format(Locale.ROOT, "%.2f", t[t.length / 2] / (double) ops) + " ns/op");
    }

    public static void main(String[] args) {
        int[] a = new int[24];
        for (int i = 0; i < a.length; i++) a[i] = i * 7;
        System.out.println("check short-loop " + shortLoop(a));
        System.out.println("check long-osr " + longOsr(1_000_000));
        counter = 0;
        System.out.println("check sync-loop " + syncLoop(1000) + " " + counter);
        System.out.println("check sync-window " + syncWindow(1000));
        System.out.println("check class-name " + className(800));
        time("short-loop-ib", 200_000, () -> { long s = 0; for (int i = 0; i < 200_000; i++) s += shortLoop(a); return s; });
        time("long-osr", 5_000_000, () -> longOsr(5_000_000));
        time("sync-loop", 1_000_000, () -> syncLoop(1_000_000));
        time("sync-window", 1_000_000, () -> syncWindow(1_000_000));
        time("class-name", 1_000_000, () -> className(1_000_000));
        time("class-hash", 1_000_000, () -> classHash(1_000_000));
        System.out.println("done " + (sink == 42 ? "" : "ok"));
    }
}
