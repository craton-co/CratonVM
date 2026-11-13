// j27 wave 6, lane T1TIERMGR: the lambda3fix book's rows
// (docs/feature-designs/jit-ir-optimizations.md), in-tree successor of the
// host-only R21Lambda3fixRows.
//
// LAMBDA3FIX21-2 (rows that do not depend on what earlier rows allocated):
//   every row runs in its OWN static loop method (no shared driver call site whose profile earlier
//   rows shape), and a heap prelude allocates and drops -Dj27.prelude.mb (default 512) MiB in 64 KiB
//   chunks before the first row, so the first allocating row does not pay the fresh heap's
//   first-touch faults (page docs/feature-designs/jit-ir-optimizations.md).
//   The creation rows run twice: `*-cold` FIRST, `*-again` LAST, with the apply rows between.
//   -Dj27.prelude.mb=0 removes the prelude (the old shape).
// LAMBDA3FIX21-1 review (scalar-replaced proxy rematerialisation):
//   apply-zeros   a capturing lambda built before the loop, capturing null / 0L / 0.0, applied 16 times;
//                 after warm-up one call publishes it from inside the loop (an uncommon branch: the
//                 deopt rematerialises the scalar-replaced proxy). The published proxy's answer is a
//                 fact: 4 on HotSpot. With the wave-5 materialiser rule (zero recipes skipped on a
//                 LEGACY proxy shell, whose cells read Int(0)) it was wrong or crashed.
//   apply-values  the same with non-zero captures ("x", 5L, 2.5): 12.
//   capture-apply-only  one int capture, the LAMBDA321-4 row.
//
// Expected output on HotSpot 25 (every arm, both modes; prelude on or off):
//   fact capture-create-cold sum=19999900000 last=600042
//   fact pair-create-cold sum=19999900000 last=200014
//   fact capture-apply-only 2240056000000
//   fact apply-zeros warm=20012700000 v=71 published=4
//   fact apply-values warm=20038300000 v=199 published=12
//   fact capture-create-again sum=19999900000 last=600042
//   fact pair-create-again sum=19999900000 last=200014
// With -Dj27.time=true also `time <row> ns/call=<n> gc-ms=<m>` lines (NOT facts).
//
// Arms (interleaved x5, P-cores, medians; --real-jdk and --compatible):
//   A    <bin>                        P0   A + -Dj27.prelude.mb=0
//   L0   <bin> CRATONVM_JIT_IR_LAMBDA_IMPL_CALL=0
//   LS0  <bin> CRATONVM_JIT_IR_LAMBDA_IMPL_LOOP_SLOT=0
//   D    <bin> CRATONVM_DBG=jit-deopt-a-lot=97:1,deopt   (facts only)
// Decides LAMBDA3FIX21-2: with the prelude, `*-cold` ~= `*-again` in each arm and A ~= L0 on the
// creation rows (P0 shows the old cold > again split). Engagement of the deopt rows:
// CRATONVM_DBG_SCALAR_DEOPT=1 2>&1 | grep -c MATERIALIZE > 0; proxy census:
// CRATONVM_DBG_JITC=1 2>&1 | grep -E 'lambda-impl-(loop-slot|site).*J27T1Lambda3fixRows' (proxy=elided for
// captureApplyOnly / applyZeros = LAMBDA3FIX21-1 holds; proxy=kept = file the use that keeps it).
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.function.IntSupplier;

public class J27T1Lambda3fixRows {
    static final boolean TIME = Boolean.getBoolean("j27.time");
    static final int PRELUDE_MB = Integer.getInteger("j27.prelude.mb", 512);
    static final int W = 200_000;
    static volatile Object escape;
    static volatile Object sink;

    static final class Pair {
        final int a;
        final int b;

        Pair(int a, int b) {
            this.a = a;
            this.b = b;
        }
    }

    // ---- kernels ----

    static int captureCreateOnly(int i, int k) {
        for (int n = 0; n < 16; n++) {
            int j = i + n;
            IntSupplier f = () -> j * k;
            escape = f;
        }
        return i;
    }

    static int pairCreateOnly(int i, int k) {
        for (int n = 0; n < 16; n++) {
            escape = new Pair(i, n);
        }
        return i;
    }

    static int captureApplyOnly(int i, int k) {
        int j = i + k;
        IntSupplier f = () -> j * 7;
        int s = 0;
        for (int n = 0; n < 16; n++) {
            s += f.getAsInt();
        }
        return s;
    }

    static int applyZeros(int i, boolean leak) {
        Object o = null;
        long l = 0L;
        double d = 0.0;
        IntSupplier f = () -> (o == null ? 1 : 2) + (int) l + (int) d + 3;
        int s = 0;
        for (int n = 0; n < 16; n++) {
            s += f.getAsInt();
            if (leak && n == 7) {
                escape = f;
            }
        }
        return s + i;
    }

    static int applyValues(int i, boolean leak) {
        Object o = "x";
        long l = 5L;
        double d = 2.5;
        IntSupplier f = () -> (o == null ? 1 : 2) + (int) l + (int) d + 3;
        int s = 0;
        for (int n = 0; n < 16; n++) {
            s += f.getAsInt();
            if (leak && n == 7) {
                escape = f;
            }
        }
        return s + i;
    }

    // ---- one loop method per row (no shared call site) ----

    static long loopCaptureCreateCold() {
        long s = 0;
        for (int i = 0; i < W; i++) s += captureCreateOnly(i, 3);
        return s;
    }

    static long loopCaptureCreateAgain() {
        long s = 0;
        for (int i = 0; i < W; i++) s += captureCreateOnly(i, 3);
        return s;
    }

    static long loopPairCreateCold() {
        long s = 0;
        for (int i = 0; i < W; i++) s += pairCreateOnly(i, 3);
        return s;
    }

    static long loopPairCreateAgain() {
        long s = 0;
        for (int i = 0; i < W; i++) s += pairCreateOnly(i, 3);
        return s;
    }

    static long loopCaptureApplyOnly() {
        long s = 0;
        for (int i = 0; i < W; i++) s += captureApplyOnly(i, 3);
        return s;
    }

    static long loopApplyZeros() {
        long s = 0;
        for (int i = 0; i < W; i++) s += applyZeros(i, false);
        return s;
    }

    static long loopApplyValues() {
        long s = 0;
        for (int i = 0; i < W; i++) s += applyValues(i, false);
        return s;
    }

    // ---- harness ----

    static long gcMillis() {
        long t = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            t += Math.max(0, b.getCollectionTime());
        }
        return t;
    }

    static void time(String row, long t0, long gc0) {
        if (TIME) {
            long ns = (System.nanoTime() - t0) / W;
            System.out.println("time " + row + " ns/call=" + ns + " gc-ms=" + (gcMillis() - gc0));
        }
    }

    static void prelude() {
        Object[] ring = new Object[16];
        int chunks = PRELUDE_MB * 16; // 64 KiB each
        for (int c = 0; c < chunks; c++) {
            byte[] b = new byte[64 * 1024];
            b[c & 0xFFFF] = 1;
            ring[c & 15] = b;
        }
        sink = ring;
        sink = null;
    }

    static void createRow(String row, boolean pair, boolean cold) {
        long gc0 = gcMillis();
        long t0 = System.nanoTime();
        long sum;
        if (pair) {
            sum = cold ? loopPairCreateCold() : loopPairCreateAgain();
        } else {
            sum = cold ? loopCaptureCreateCold() : loopCaptureCreateAgain();
        }
        time(row, t0, gc0);
        long last = pair ? ((Pair) escape).a + ((Pair) escape).b : ((IntSupplier) escape).getAsInt();
        System.out.println("fact " + row + " sum=" + sum + " last=" + last);
    }

    public static void main(String[] args) {
        if (PRELUDE_MB > 0) {
            prelude();
        }
        createRow("capture-create-cold", false, true);
        createRow("pair-create-cold", true, true);

        long gc0 = gcMillis();
        long t0 = System.nanoTime();
        long apply = loopCaptureApplyOnly();
        time("capture-apply-only", t0, gc0);
        System.out.println("fact capture-apply-only " + apply);

        gc0 = gcMillis();
        t0 = System.nanoTime();
        long zeros = loopApplyZeros();
        time("apply-zeros", t0, gc0);
        int vz = applyZeros(7, true);
        System.out.println("fact apply-zeros warm=" + zeros + " v=" + vz
                + " published=" + ((IntSupplier) escape).getAsInt());

        gc0 = gcMillis();
        t0 = System.nanoTime();
        long vals = loopApplyValues();
        time("apply-values", t0, gc0);
        int vv = applyValues(7, true);
        System.out.println("fact apply-values warm=" + vals + " v=" + vv
                + " published=" + ((IntSupplier) escape).getAsInt());

        createRow("capture-create-again", false, false);
        createRow("pair-create-again", true, false);
    }
}
