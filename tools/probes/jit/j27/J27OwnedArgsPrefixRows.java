// j27 wave 5 (lane OOM21): owned-argument callers whose prefix stores a CONSTANT into a static.
//
// Pages: docs/feature-designs/jit-ir-optimizations.md
//        docs/feature-designs/native-dispatch-architecture.md
//
// Retention rows (only with -Dj27.big=true; `fact <row>-oome <bool>`): each driver warms its
// caller with small arrays, then passes one array of 55% of the heap that `drop` stops using before
// it allocates a second one. HotSpot 25 roots neither (every row `false`).
//   store     stat = null (plain static, constant)        rule admits under the switch
//   long      lstat = 1L (two-word constant)                rule admits under the switch
//   volatile  vstat = null (volatile static)                refused (CratonVM stays `true`)
//   param     istat = size (not a constant)                 refused (CratonVM stays `true`)
//
// Correctness facts (always printed, must equal HotSpot 25 byte for byte):
//   fact small-sum 358800
//   fact stat-null true
//   fact lstat 1
//   fact vstat-null true
//   fact istat 299
//   fact throw-sum 1136558075 throws=4545
//   fact trap-sum 1250674997
//   fact istat-after-trap 3
//   fact gc-sum 1250285944
//   fact handoff-sum 1251451000
// Expected HotSpot 25 output with -Dj27.big=true: the four `fact <row>-oome false` lines first
// (store, long, volatile, param), then the facts above.
//
// `handoff-sum` is the hazard row: outerCaller (refused site: non-constant putstatic) passes a
// reference to midCaller (owned site: its prefix fills its own array), whose callee collects
// and then takes a cold branch for the first time on the last iteration. If that trap is
// DECLINED (not resumed precisely) after the collector cleared outerCaller's service copy while
// midCaller sat at its call, a re-run of midCaller from those copies reads null and throws
// NullPointerException. A precise resume (the usual answer) leaves the row a plain guard.
//
// Run (build: javac -d tools/probes/out tools/probes/jit/j27/J27OwnedArgsPrefixRows.java):
//   java -Xmx96m -XX:-UseCompressedOops -cp $P -Dj27.big=true J27OwnedArgsPrefixRows    # HotSpot 25
//   cratonvm -Xmx96m -cp $P -Dj27.big=true J27OwnedArgsPrefixRows
//     x {default, CRATONVM_JIT_IR_INLINE=0, CRATONVM_C2_ACCEPT=never}
//     x {rule off, CRATONVM_JIT_OWNED_ARGS_CONST_STATIC_STORES=1 CRATONVM_JIT_SERVICE_COPY_WINDOW=0}
//     x {--real-jdk, --compatible} x {ZGC, -XX:+UseG1GC, -XX:+UseGenerationalGC}
// Engagement: CRATONVM_DBG_IR_SLOTS=1 prints `[ir-owned-args] site bci=10
// J27OwnedArgsPrefixRows.drop([BI)I kind=3 admitted=true owned=true` for storeCaller/longCaller.

public class J27OwnedArgsPrefixRows {
    static final int BIG = (int) (Runtime.getRuntime().maxMemory() * 0.55);
    static final int WARM = 50_000;
    static final int SMALL = 16;
    static final int COLD_FROM = WARM - 100;
    static final int HAZ_FROM = WARM - 1;
    static final Object[] BOXES = {11, 22, 33, 44};

    static Object stat = "init";
    static volatile Object vstat = "init";
    static long lstat;
    static int istat;
    static int istat2;

    // ---- the callee: use the argument, drop it, allocate ----

    static int drop(byte[] p, int size) {
        int n = p.length;
        byte[] q = new byte[size];
        q[0] = 1;
        return n + q.length;
    }

    // ---- callers (straight-line prefixes; the call is at bci 10) ----

    static int storeCaller(int size) {
        byte[] a = new byte[size];
        stat = null;
        return drop(a, size);
    }

    static int longCaller(int size) {
        byte[] a = new byte[size];
        lstat = 1L;
        return drop(a, size);
    }

    static int volatileCaller(int size) {
        byte[] a = new byte[size];
        vstat = null;
        return drop(a, size);
    }

    static int paramCaller(int size) {
        byte[] a = new byte[size];
        istat = size;
        return drop(a, size);
    }

    interface Row {
        int run(int size);
    }

    // The big call is the loop's LAST iteration, so it runs in the warmed code.
    static long drive(Row r, int big) {
        long s = 0;
        for (int i = 0; i <= WARM; i++) {
            s += r.run(i == WARM ? big : SMALL);
        }
        return s;
    }

    static void oome(String name, Row r) {
        boolean threw = false;
        long s = 0;
        try {
            s = drive(r, BIG);
        } catch (OutOfMemoryError e) {
            threw = true;
        }
        System.out.println("fact " + name + "-oome " + threw);
        System.gc();
        if (s == 42) {
            System.out.println("unreachable");
        }
    }

    // ---- exception row ----

    static int thrower(byte[] p, int k) {
        if (k % 11 == 5) {
            throw new IllegalStateException("k=" + k + " len=" + p.length);
        }
        return p.length + k;
    }

    static int throwCaller(int k) {
        byte[] a = new byte[k % 5 + 1];
        stat = null;
        return thrower(a, k);
    }

    // ---- deopt row: a branch first taken after warm-up ----

    static int coldPath(byte[] p, int k) {
        int n = p.length;
        if (k >= COLD_FROM) {
            n += String.valueOf(k).length() * 1000;
        }
        return n + k;
    }

    static int trapCaller(int k) {
        byte[] a = new byte[k % 7 + 1];
        istat = 3;
        return coldPath(a, k);
    }

    // ---- GC under the call: the callee reads its argument after a collection ----

    static int useAfterGc(byte[] p, int k) {
        if (k % 5000 == 0) {
            System.gc();
        }
        int s = 0;
        for (byte b : p) {
            s += b;
        }
        return s + k;
    }

    static int gcCaller(int k) {
        byte[] a = new byte[3];
        a[0] = (byte) k;
        a[1] = (byte) (k >> 1);
        a[2] = 7;
        stat = null;
        return useAfterGc(a, k);
    }

    // ---- hand-off hazard row ----

    static int syncCold(byte[] p, int k) {
        if (k % 1000 == 999) {
            System.gc();
        }
        int n = p.length;
        if (k >= HAZ_FROM) {
            n += 1000;
        }
        return n + k;
    }

    static int midCaller(Object o, int k) {
        byte[] a = new byte[2];
        a[0] = 1;
        int r = syncCold(a, k);
        return r + ((Integer) o).intValue();
    }

    static int outerCaller(int k) {
        Object o = BOXES[k & 3];
        istat2 = k;
        return midCaller(o, k);
    }

    public static void main(String[] args) {
        if (Boolean.getBoolean("j27.big")) {
            oome("store", J27OwnedArgsPrefixRows::storeCaller);
            oome("long", J27OwnedArgsPrefixRows::longCaller);
            oome("volatile", J27OwnedArgsPrefixRows::volatileCaller);
            oome("param", J27OwnedArgsPrefixRows::paramCaller);
        }
        stat = "init";
        vstat = "init";
        long small = 0;
        Row[] rows = {
            J27OwnedArgsPrefixRows::storeCaller, J27OwnedArgsPrefixRows::longCaller,
            J27OwnedArgsPrefixRows::volatileCaller, J27OwnedArgsPrefixRows::paramCaller,
        };
        for (Row r : rows) {
            for (int s = 1; s < 300; s++) {
                small += r.run(s);
            }
        }
        System.out.println("fact small-sum " + small);
        System.out.println("fact stat-null " + (stat == null));
        System.out.println("fact lstat " + lstat);
        System.out.println("fact vstat-null " + (vstat == null));
        System.out.println("fact istat " + istat);

        long thrown = 0;
        int throwsSeen = 0;
        for (int k = 0; k < WARM; k++) {
            try {
                thrown += throwCaller(k);
            } catch (IllegalStateException e) {
                throwsSeen++;
                thrown += e.getMessage().length();
            }
        }
        System.out.println("fact throw-sum " + thrown + " throws=" + throwsSeen);

        long trap = 0;
        for (int k = 0; k < WARM; k++) {
            trap += trapCaller(k);
        }
        System.out.println("fact trap-sum " + trap);
        System.out.println("fact istat-after-trap " + istat);

        long gc = 0;
        for (int k = 0; k < WARM; k++) {
            gc += gcCaller(k);
        }
        System.out.println("fact gc-sum " + gc);

        long handoff = 0;
        for (int k = 0; k < WARM; k++) {
            handoff += outerCaller(k);
        }
        System.out.println("fact handoff-sum " + handoff);
    }
}
