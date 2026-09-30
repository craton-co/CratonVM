// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT-vs-interpreter differential exercise program.
//
// Driven by `vm/tests/jit_interp_differential.rs`, which runs this class
// twice on the `cratonvm` CLI — once with `CRATONVM_DISABLE_JIT=1` (pure
// interpreter) and once with the JIT enabled and forced to compile the
// `cratonvm/*` package eagerly (`CRATONVM_JIT_ALLOW_PACKAGES=cratonvm/`,
// `CRATONVM_JIT_THRESHOLD=1`). The two stdout streams must be byte-for-byte
// identical; any divergence is a JIT miscompile.
//
// Design notes
// ------------
//  * Every observable result is emitted on a line prefixed with `r:` so the
//    Rust harness can isolate the comparable observations from incidental
//    log noise. The final line is the `JIT_DIFFERENTIAL_OK <n>` marker.
//  * The arithmetic / FP / shift / division / array / call kernels are
//    factored into small `static` helper methods that are deliberately HOT:
//    `main` first warms each kernel in a loop so the JIT (threshold 1 in the
//    JIT run) compiles them, THEN replays each kernel over the edge-case
//    input matrix and prints the result. This guarantees the printed values
//    come from JIT'd code in the JIT run and from the interpreter in the
//    interpreter run — the whole point of the differential.
//  * The kernels are chosen to surface the historical miscompile classes:
//      - call-argument escape analysis (bug-25): `sink`/`viaCall` pass a
//        freshly-allocated object as a call argument; if escape analysis
//        wrongly scalar-replaces it the result diverges or NPEs.
//      - SETcc / comparison lowering: `cmp*` return 0/1 from <, <=, ==, etc.
//        over signed boundary values, exercising the ir_lower SETcc path.
//      - aastore / array bounds: `arrStore`/`arrSum` round-trip values
//        through an Object[] and an int[], and probe AIOOBE on both ends.
//      - idiv/ldiv MIN_VALUE / -1 overflow, shift masking, FP NaN / -0.0.
//
// This is a plain (non-`// JAVA21+`) source: `vm/build.rs` compiles it with
// legacy `javac` to `tests/resources/cratonvm/JitDifferential.class`.

package cratonvm;

public class JitDifferential {

    // -- result sink -------------------------------------------------------
    // A running observation counter so the harness can assert the program
    // emitted the full matrix (not aborted half-way through a JIT'd kernel).
    private static int observations = 0;

    private static void r(String label, long value) {
        System.out.println("r:" + label + "=" + value);
        observations++;
    }

    private static void r(String label, String value) {
        System.out.println("r:" + label + "=" + value);
        observations++;
    }

    // ----------------------------------------------------------------------
    // Integer arithmetic kernels (incl. overflow / MIN_VALUE)
    // ----------------------------------------------------------------------

    static int iadd(int a, int b) { return a + b; }
    static int isub(int a, int b) { return a - b; }
    static int imul(int a, int b) { return a * b; }
    static int ineg(int a)        { return -a; }

    // idiv / irem — the JVM-mandated special case is MIN_VALUE / -1, which
    // overflows to MIN_VALUE (no exception) and MIN_VALUE % -1 == 0. A naive
    // x86 `idiv` lowering that does not special-case it raises #DE.
    static int idiv(int a, int b) { return a / b; }
    static int irem(int a, int b) { return a % b; }

    // ----------------------------------------------------------------------
    // Long arithmetic kernels
    // ----------------------------------------------------------------------

    static long ladd(long a, long b) { return a + b; }
    static long lmul(long a, long b) { return a * b; }
    static long ldiv(long a, long b) { return a / b; }
    static long lrem(long a, long b) { return a % b; }

    // ----------------------------------------------------------------------
    // Shift masking — the shift amount is masked to 5 bits (int) / 6 bits
    // (long) by the JVM spec. A lowering that forwards the raw count to the
    // hardware shift (which masks differently or UBs) diverges for counts
    // >= 32 / >= 64.
    // ----------------------------------------------------------------------

    static int  ishl(int a, int s)   { return a << s; }
    static int  ishr(int a, int s)   { return a >> s; }
    static int  iushr(int a, int s)  { return a >>> s; }
    static long lshl(long a, int s)  { return a << s; }
    static long lushr(long a, int s) { return a >>> s; }

    // ----------------------------------------------------------------------
    // Comparison / SETcc lowering — returns 0/1 so an inverted condition or
    // a wrong signed/unsigned compare shows up as a flipped bit.
    // ----------------------------------------------------------------------

    static int cmpLt(int a, int b)  { return a < b  ? 1 : 0; }
    static int cmpLe(int a, int b)  { return a <= b ? 1 : 0; }
    static int cmpEq(int a, int b)  { return a == b ? 1 : 0; }
    static int cmpGe(int a, int b)  { return a >= b ? 1 : 0; }
    static int lcmp(long a, long b) { return a < b ? 1 : (a == b ? 0 : -1); }

    // ----------------------------------------------------------------------
    // Floating point — NaN, -0.0, infinities. Compared via raw bits so that
    // -0.0 vs +0.0 and the canonical NaN bit pattern are distinguished.
    // ----------------------------------------------------------------------

    static double dadd(double a, double b) { return a + b; }
    static double dmul(double a, double b) { return a * b; }
    static double ddiv(double a, double b) { return a / b; }
    static float  fadd(float a, float b)   { return a + b; }
    static float  fmul(float a, float b)   { return a * b; }

    // dcmpg / dcmpl distinguish on NaN: dcmpg returns +1, dcmpl returns -1.
    static int dcmpgLt(double a, double b) { return a < b ? 1 : 0; }   // dcmpg path
    static int dcmplGt(double a, double b) { return a > b ? 1 : 0; }   // dcmpl path

    // ----------------------------------------------------------------------
    // Array load / store + bounds. `aastore` covariance and int[] element
    // round-trip; both ends of the bounds are probed for AIOOBE.
    // ----------------------------------------------------------------------

    static int arrInt(int[] a, int i) { return a[i]; }

    static String arrStore(int idx) {
        Object[] a = new Object[4];
        try {
            a[idx] = "x";          // aastore; idx out of range -> AIOOBE
            return (String) a[idx]; // aaload
        } catch (ArrayIndexOutOfBoundsException e) {
            return "AIOOBE";
        }
    }

    static int arrIntBounds(int[] a, int idx) {
        try {
            return a[idx];
        } catch (ArrayIndexOutOfBoundsException e) {
            return -999;
        }
    }

    // ----------------------------------------------------------------------
    // Method-call kernels — the bug-25 class. `viaCall` allocates a small
    // holder and passes it AS A CALL ARGUMENT to `sink`. A JIT escape pass
    // that wrongly treats the holder as non-escaping (because the call arg's
    // provenance was lost) would elide the allocation / skip <init> and read
    // a null/garbage field, diverging from the interpreter.
    // ----------------------------------------------------------------------

    static final class Holder {
        final int v;
        Holder(int v) { this.v = v; }
    }

    static int sink(Holder h, int bias) { return h.v + bias; }

    static int viaCall(int seed) {
        Holder h = new Holder(seed * 3 + 1);
        // The fresh Holder escapes as a call argument; must not be scalar-
        // replaced away.
        return sink(h, seed);
    }

    // Simple counted loop with a data-dependent accumulation — a loop the
    // JIT will try to OSR / optimize. Independent Rust-side oracle not
    // needed: the interpreter run IS the oracle.
    static long loopSum(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += (long) i * (i & 7);
        }
        return acc;
    }

    // ----------------------------------------------------------------------
    // Warmup — call every kernel enough times that, with the JIT threshold
    // forced to 1 in the JIT run, the method is compiled before the edge-
    // case replay below records its result. The warmup inputs are benign
    // (no division by zero) so warmup itself never throws.
    // ----------------------------------------------------------------------

    // ----------------------------------------------------------------------
    // Static-field reads (getstatic) — the inline direct-load type matrix
    // ----------------------------------------------------------------------
    // A compiled `getstatic` is a direct load against the declaring class's
    // statics block, not a `jit_getstatic` call
    // (jit-getstatic-costs-a-helper-call-FIXED-20260803.md). The
    // load WIDTH and EXTENSION are picked from the field's descriptor: MOVSXD
    // for the int category, a 32-bit zero-extending MOV for float, a 64-bit MOV
    // for long/double/reference. Every rung below is therefore a negative, a
    // boundary, or a bit pattern whose high half matters — a wrong width or a
    // wrong extension is invisible for small positive ints and wrong for
    // everything else.
    static boolean sZ = true;
    static byte sB = -128;
    static char sC = (char) 0xFFFF;   // spelled numerically: the fixture is
                                      // compiled by whatever javac encoding
                                      // the build host defaults to
    static short sS = -32768;
    static int sI = Integer.MIN_VALUE;
    static long sJ = Long.MIN_VALUE;
    static float sF = -0.0f;
    static double sD = Double.NaN;
    static String sRef = "static-ref";
    static int[] sArr = { 7, -7 };
    static volatile int sVol = -1;
    // Written AFTER the getter is compiled: proves the baked address is the
    // live slot and not a snapshot (see `StaticsIndex::base_cell_addr`).
    static int sMut = 0;

    private static long getZ() { return sZ ? 1L : 0L; }
    private static long getB() { return sB; }
    private static long getC() { return sC; }
    private static long getS() { return sS; }
    private static long getI() { return sI; }
    private static long getJ() { return sJ; }
    private static long getFBits() { return Float.floatToRawIntBits(sF); }
    private static long getDBits() { return Double.doubleToRawLongBits(sD); }
    private static String getRef() { return sRef; }
    private static long getRefLen() { return sRef.length(); }
    private static long getArr1() { return sArr[1]; }
    private static long getVol() { return sVol; }
    private static long getMut() { return sMut; }

    private static void warmup() {
        long blackhole = 0;
        for (int k = 0; k < 4000; k++) {
            blackhole += iadd(k, 1);
            blackhole += isub(k, 1);
            blackhole += imul(k, 3);
            blackhole += ineg(k);
            blackhole += idiv(k + 1, 2);
            blackhole += irem(k + 1, 3);
            blackhole += ladd(k, 7L);
            blackhole += lmul(k, 9L);
            blackhole += ldiv(k + 1, 4L);
            blackhole += lrem(k + 1, 5L);
            blackhole += ishl(k, k & 31);
            blackhole += ishr(-k, k & 31);
            blackhole += iushr(-k, k & 31);
            blackhole += lshl(k, k & 63);
            blackhole += lushr(-k, k & 63);
            blackhole += cmpLt(k, 2000);
            blackhole += cmpLe(k, 2000);
            blackhole += cmpEq(k, 2000);
            blackhole += cmpGe(k, 2000);
            blackhole += lcmp(k, 2000L);
            blackhole += Double.doubleToRawLongBits(dadd(k, 0.5));
            blackhole += Double.doubleToRawLongBits(dmul(k, 1.5));
            blackhole += Double.doubleToRawLongBits(ddiv(k + 1, 2.0));
            blackhole += Float.floatToRawIntBits(fadd(k, 0.25f));
            blackhole += Float.floatToRawIntBits(fmul(k, 0.5f));
            blackhole += dcmpgLt(k, 2000.0);
            blackhole += dcmplGt(k, 2000.0);
            blackhole += viaCall(k);
            blackhole += loopSum(8);
            blackhole += arrIntBounds(WARM_ARR, k & 7);
            blackhole += getZ() + getB() + getC() + getS() + getI() + getJ();
            blackhole += getFBits() + getDBits() + getRefLen() + getArr1();
            blackhole += getVol() + getMut();
        }
        // Defeat dead-code elimination of the warmup so the kernels really
        // run (and so a JIT that prematurely DCE'd them would be caught).
        if (blackhole == Long.MIN_VALUE) {
            System.out.println("warmup-sentinel");
        }
    }

    private static final int[] WARM_ARR = { 0, 1, 2, 3, 4, 5, 6, 7 };

    // ----------------------------------------------------------------------
    // Edge-case replay matrix.
    // ----------------------------------------------------------------------

    private static void replayIntArith() {
        r("iadd.maxplus1", iadd(Integer.MAX_VALUE, 1));        // overflow -> MIN
        r("iadd.minminus1", iadd(Integer.MIN_VALUE, -1));      // underflow -> MAX
        r("isub.minminus1", isub(Integer.MIN_VALUE, 1));       // underflow
        r("imul.bigsquare", imul(46341, 46341));               // 32-bit wrap
        r("imul.minby2", imul(Integer.MIN_VALUE, 2));          // -> 0
        r("ineg.min", ineg(Integer.MIN_VALUE));                // -> MIN (no flip)
        // The mandated overflow special case.
        r("idiv.min_by_-1", idiv(Integer.MIN_VALUE, -1));      // -> MIN_VALUE
        r("irem.min_by_-1", irem(Integer.MIN_VALUE, -1));      // -> 0
        r("idiv.7_by_-2", idiv(7, -2));                        // truncation toward 0
        r("irem.-7_by_3", irem(-7, 3));                        // sign of dividend
        r("irem.7_by_-3", irem(7, -3));
    }

    private static void replayLongArith() {
        r("ladd.maxplus1", ladd(Long.MAX_VALUE, 1L));
        r("lmul.bigsquare", lmul(3037000500L, 3037000500L));   // 64-bit wrap
        r("ldiv.min_by_-1", ldiv(Long.MIN_VALUE, -1L));        // -> MIN (overflow)
        r("lrem.min_by_-1", lrem(Long.MIN_VALUE, -1L));        // -> 0
        r("ldiv.-9_by_4", ldiv(-9L, 4L));
        r("lrem.-9_by_4", lrem(-9L, 4L));
    }

    private static void replayShifts() {
        // Counts at / beyond the mask boundary.
        r("ishl.1_by_32", ishl(1, 32));        // masks to 0 -> 1
        r("ishl.1_by_33", ishl(1, 33));        // masks to 1 -> 2
        r("ishr.min_by_31", ishr(Integer.MIN_VALUE, 31));   // -> -1 (sign fill)
        r("iushr.min_by_31", iushr(Integer.MIN_VALUE, 31)); // -> 1 (zero fill)
        r("iushr.-1_by_0", iushr(-1, 0));      // -> -1 (no shift)
        r("lshl.1_by_64", lshl(1L, 64));       // masks to 0 -> 1
        r("lshl.1_by_65", lshl(1L, 65));       // masks to 1 -> 2
        r("lushr.min_by_63", lushr(Long.MIN_VALUE, 63));     // -> 1
    }

    private static void replayCompares() {
        r("cmpLt.min_lt_max", cmpLt(Integer.MIN_VALUE, Integer.MAX_VALUE)); // 1
        r("cmpLt.max_lt_min", cmpLt(Integer.MAX_VALUE, Integer.MIN_VALUE)); // 0
        r("cmpLe.eq", cmpLe(7, 7));                                         // 1
        r("cmpEq.eq", cmpEq(-5, -5));                                       // 1
        r("cmpEq.ne", cmpEq(-5, 5));                                        // 0
        r("cmpGe.min_ge_min", cmpGe(Integer.MIN_VALUE, Integer.MIN_VALUE)); // 1
        r("lcmp.lt", lcmp(Long.MIN_VALUE, Long.MAX_VALUE));                 // 1
        r("lcmp.gt", lcmp(Long.MAX_VALUE, Long.MIN_VALUE));                 // -1
        r("lcmp.eq", lcmp(0L, 0L));                                         // 0
    }

    private static void replayFloating() {
        // Compared via raw bits: -0.0, NaN, infinities are all distinguished.
        r("dadd.bits", Double.doubleToRawLongBits(dadd(0.1, 0.2)));
        r("dmul.negzero", Double.doubleToRawLongBits(dmul(-1.0, 0.0)));     // -0.0
        r("ddiv.1_by_0", Double.doubleToRawLongBits(ddiv(1.0, 0.0)));       // +Inf
        r("ddiv.-1_by_0", Double.doubleToRawLongBits(ddiv(-1.0, 0.0)));     // -Inf
        r("ddiv.0_by_0", Double.doubleToRawLongBits(ddiv(0.0, 0.0)));       // NaN
        r("dmul.nan", Double.doubleToRawLongBits(dmul(Double.NaN, 1.0)));   // NaN
        r("fadd.bits", Float.floatToRawIntBits(fadd(0.1f, 0.2f)));
        r("fmul.negzero", Float.floatToRawIntBits(fmul(-1.0f, 0.0f)));      // -0.0f
        // NaN comparison: dcmpg (a<b) yields false for NaN; dcmpl (a>b) too.
        r("dcmpg.nan_lt_1", dcmpgLt(Double.NaN, 1.0));                      // 0
        r("dcmpl.nan_gt_1", dcmplGt(Double.NaN, 1.0));                      // 0
        r("dcmpg.neg0_lt_pos0", dcmpgLt(-0.0, 0.0));                        // 0 (equal)
    }

    private static void replayArrays() {
        int[] a = { 10, 20, 30 };
        r("arrInt.0", arrInt(a, 0));
        r("arrInt.2", arrInt(a, 2));
        r("arrStore.inbounds", arrStore(2));      // "x"
        r("arrStore.oob_high", arrStore(4));      // AIOOBE
        r("arrStore.oob_neg", arrStore(-1));      // AIOOBE
        r("arrIntBounds.ok", arrIntBounds(a, 1)); // 20
        r("arrIntBounds.high", arrIntBounds(a, 3)); // -999 (AIOOBE caught)
        r("arrIntBounds.neg", arrIntBounds(a, -1)); // -999
    }

    private static void replayCalls() {
        // bug-25 class: fresh allocation passed as a call arg.
        r("viaCall.0", viaCall(0));     // sink(Holder(1), 0) = 1
        r("viaCall.5", viaCall(5));     // sink(Holder(16), 5) = 21
        r("viaCall.neg", viaCall(-2));  // sink(Holder(-5), -2) = -7
        r("loopSum.0", loopSum(0));     // 0
        r("loopSum.10", loopSum(10));
        r("loopSum.100", loopSum(100));
    }

    private static void replayStatics() {
        r("static.Z", getZ());                 // 1
        r("static.B", getB());                 // -128
        r("static.C", getC());                 // 65535 (char is unsigned)
        r("static.S", getS());                 // -32768
        r("static.I", getI());                 // Integer.MIN_VALUE
        r("static.J", getJ());                 // Long.MIN_VALUE — also the
                                               // helper path's deopt sentinel
        r("static.F.bits", getFBits());        // -0.0f
        r("static.D.bits", getDBits());        // NaN
        r("static.ref", getRef());             // "static-ref"
        r("static.ref.len", getRefLen());      // 10
        r("static.arr.1", getArr1());          // -7
        r("static.vol", getVol());             // -1

        // A store after the getter is hot: the compiled read must observe it.
        sMut = 12345;
        r("static.mut.a", getMut());
        sMut = Integer.MIN_VALUE;
        r("static.mut.b", getMut());
        sMut = -1;
        r("static.mut.c", getMut());
    }

    public static void main(String[] args) {
        // Warm the kernels so the JIT run compiles them before replay.
        warmup();

        replayIntArith();
        replayLongArith();
        replayShifts();
        replayCompares();
        replayFloating();
        replayArrays();
        replayCalls();
        replayStatics();

        // Completion marker — the harness asserts this is present so a
        // mid-replay abort (e.g. a JIT #DE on idiv MIN/-1) is caught as a
        // missing marker rather than silently passing.
        System.out.println("JIT_DIFFERENTIAL_OK " + observations);
    }
}
