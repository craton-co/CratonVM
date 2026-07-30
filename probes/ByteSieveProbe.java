/**
 * Differential probe for the three guarded byte-array loop lowerings added by
 * `perf(jit): specialize counted byte sieve loops`:
 *
 *   * `emit_bulk_zero_byte_fill_preheader`   — `for (i=0; i<=n; i++) a[i]=false;`
 *   * `emit_bulk_set_byte_stride_preheader`  — `for (j=s; j<=n; j+=step) a[j]=true;`
 *   * `emit_byte_sieve_preheader`            — the whole sieve loop nest
 *
 * Every kernel below has the EXACT canonical bytecode shape the detectors
 * accept, so each one is really lowered; the parameters then push it through
 * the guard edges (null array, short array, negative/zero bound, span over the
 * 1 MiB cap, `i+i` overflow, a pre-dirtied array) where the emitted code must
 * branch back to the original bytecode with untouched state.
 *
 * Run the same class three ways and diff the output:
 *   cratonvm                                    (JIT + lowering)
 *   cratonvm --nojit                            (interpreter reference)
 *   CRATONVM_JIT_BULK_BYTE_LOOPS=0 cratonvm     (JIT, lowering off)
 *   java                                        (HotSpot reference)
 *
 * Each line is `name=value`; any drift between the four is a defect.
 */
public class ByteSieveProbe {

    // --- the exact CratonBench shapes ---------------------------------

    static int sieve(boolean[] composite, int limit) {
        for (int i = 0; i <= limit; i++) {
            composite[i] = false;
        }
        int count = 0;
        for (int i = 2; i <= limit; i++) {
            if (!composite[i]) {
                count++;
                for (int j = i + i; j <= limit; j += i) {
                    composite[j] = true;
                }
            }
        }
        return count;
    }

    /** Bulk zero fill in isolation. */
    static int fill(boolean[] a, int limit) {
        for (int i = 0; i <= limit; i++) {
            a[i] = false;
        }
        int n = 0;
        for (int i = 0; i < a.length; i++) {
            if (a[i]) n++;
        }
        return n;
    }

    /** Strided set in isolation. */
    static int stride(boolean[] a, int start, int limit, int step) {
        for (int j = start; j <= limit; j += step) {
            a[j] = true;
        }
        int n = 0;
        for (int i = 0; i < a.length; i++) {
            if (a[i]) n++;
        }
        return n;
    }

    static String call(String name, java.util.function.Supplier<String> body) {
        String r;
        try {
            r = body.get();
        } catch (Throwable t) {
            r = t.getClass().getName();
        }
        return name + "=" + r;
    }

    static String checksum(boolean[] a) {
        // Order-sensitive so a mis-set element cannot cancel a missing one.
        long h = 1469598103934665603L;
        for (int i = 0; i < a.length; i++) {
            h ^= a[i] ? 1 : 0;
            h *= 1099511628211L;
        }
        return Long.toHexString(h);
    }

    static void line(String s) {
        System.out.println(s);
    }

    public static void main(String[] args) {
        // Warm every kernel past the JIT/OSR threshold so the lowered code —
        // not the interpreter — produces the reported values.
        boolean[] warm = new boolean[1001];
        int w = 0;
        for (int r = 0; r < 3000; r++) {
            w += sieve(warm, 1000);
            w += fill(warm, 1000);
            w += stride(warm, 2, 1000, 3);
        }
        line("warm=" + (w != 0));

        // 1. The real benchmark shape and checksum.
        boolean[] a = new boolean[100001];
        line("sieve.100k=" + sieve(a, 100000));
        line("sieve.100k.arr=" + checksum(a));

        // 2. Small exact cases, including limits below the 8-byte word scan.
        for (int lim : new int[] {0, 1, 2, 3, 7, 8, 9, 15, 16, 30, 63, 64, 65}) {
            boolean[] s = new boolean[lim + 1];
            line("sieve." + lim + "=" + sieve(s, lim) + ":" + checksum(s));
        }

        // 3. A pre-dirtied array: the zero-fill must actually clear it, and
        //    the sieve's word scan must not treat stale 1s as composite.
        boolean[] dirty = new boolean[201];
        for (int i = 0; i < dirty.length; i++) dirty[i] = true;
        line("sieve.dirty200=" + sieve(dirty, 200) + ":" + checksum(dirty));

        // 4. Guard edges — every one must reach the original bytecode and
        //    reproduce its exception / no-op behaviour exactly.
        line(call("sieve.null", () -> String.valueOf(sieve(null, 100))));
        line(call("sieve.short", () -> {
            boolean[] s = new boolean[10];
            return String.valueOf(sieve(s, 100));           // AIOOBE in the fill
        }));
        line(call("sieve.limitNeg", () -> {
            boolean[] s = new boolean[8];
            return sieve(s, -1) + ":" + checksum(s);        // zero-trip everywhere
        }));
        line(call("sieve.limitIsLen", () -> {
            boolean[] s = new boolean[8];
            return String.valueOf(sieve(s, 8));             // AIOOBE at the last fill
        }));
        line(call("sieve.exactLast", () -> {
            boolean[] s = new boolean[9];
            return sieve(s, 8) + ":" + checksum(s);         // limit == len-1, in range
        }));

        // 5. Fill guard edges.
        line(call("fill.null", () -> String.valueOf(fill(null, 4))));
        line(call("fill.neg", () -> {
            boolean[] s = new boolean[4];
            s[0] = true;
            return fill(s, -1) + ":" + checksum(s);         // must NOT clear s[0]
        }));
        line(call("fill.over", () -> {
            boolean[] s = new boolean[4];
            return String.valueOf(fill(s, 4));              // AIOOBE
        }));
        line(call("fill.big", () -> {
            boolean[] s = new boolean[(1 << 21) + 1];       // span > MAX_BULK span
            s[7] = true;
            return fill(s, (1 << 21)) + ":" + checksum(s);
        }));

        // 6. Stride guard edges.
        line(call("stride.null", () -> String.valueOf(stride(null, 2, 10, 2))));
        // NOTE: `step == 0` with `start <= limit` is an infinite loop in Java
        // itself, so there is no reference behaviour to diff against; the
        // emitted guard rejects it, and the zero-trip form below is what can
        // actually be compared.
        line(call("stride.zeroStepEmpty", () -> {
            boolean[] s = new boolean[8];
            return stride(s, 5, 4, 0) + ":" + checksum(s);  // zero-trip, step 0
        }));
        line(call("stride.negStep", () -> {
            boolean[] s = new boolean[8];
            return String.valueOf(stride(s, 2, 4, -1));     // j decreases: AIOOBE at -1
        }));
        line(call("stride.negStart", () -> {
            boolean[] s = new boolean[8];
            return String.valueOf(stride(s, -1, 4, 2));     // AIOOBE immediately
        }));
        line(call("stride.overflow", () -> {
            boolean[] s = new boolean[8];
            // step near INT_MAX: `j += step` wraps negative, so Java exits or
            // faults rather than looping forever.
            return stride(s, 2, 4, Integer.MAX_VALUE - 1) + ":" + checksum(s);
        }));
        line(call("stride.past", () -> {
            boolean[] s = new boolean[8];
            return String.valueOf(stride(s, 2, 9, 3));      // limit >= len: AIOOBE at 8
        }));
        line(call("stride.exact", () -> {
            boolean[] s = new boolean[8];
            return stride(s, 2, 7, 5) + ":" + checksum(s);
        }));

        // 7. `i + i` overflow inside the sieve's inner-loop start expression:
        //    with a huge limit the JIT must not fabricate a positive start.
        line(call("sieve.hugeLimit", () -> {
            boolean[] s = new boolean[16];
            return String.valueOf(sieve(s, Integer.MAX_VALUE));
        }));
    }
}
