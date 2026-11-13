// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 4 (lane MHROWS): the MethodHandle rows of
// docs/feature-designs/jit-ir-optimizations.md (MH6-1, MH7-4, MH16-7, MHB-2, MHF-3).
// Checks the answers the compiled constant bind must keep:
//   * bound STATIC handles (findStatic(..).bindTo(x), MH16-7,
//     CRATONVM_JIT_MH_CONSTANT_BIND_REAL_BOUND_STATIC) over Long, Short, Character, Boolean,
//     Object (null and a String) leading parameters, constant, alternating and bound per call;
//   * converting handles (asType((Object)Object) over long / short / char / boolean members,
//     MH6-1's shape) with Long, Integer, Short, Byte, Character, Boolean and null arguments;
//   * the ClassCastException / WrongMethodTypeException / IllegalArgumentException messages;
//   * constant-handle compiles (exact static, unbound virtual, bound virtual: MH7-4).
// j27 wave 5 (lane MHASTYPE, CRATONVM_JIT_MH_CONSTANT_BIND_REAL_CONVERTING): hot real asType
// results with faults (CCE text, NPE), Long.valueOf identity, Long.MIN_VALUE results, a
// (Number)Integer static and an (Acc,Object)Object virtual member.
// j27 wave 6 (lane T1LAM2, CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC=1, opt-in): bound statics
// over a mutable Box compiled as invokestatic(argL1, a*): the live bound object, a throw inside
// the member (message + top two frames), the member's class initialised by the first invokeExact,
// and the WMTE / type of the bound handle. Expected HotSpot 25 lines (derived from JDK 25):
//   fact bound-static-clinit false true 1 java.lang.String
//   fact bound-static-live-sum 51381728 same=true
//   fact bound-static-throw-hot 12728964100000 same=true
//   fact bound-static-lazy-sum 2400000 same=true
//   fact bound-static-box-wmte java.lang.invoke.WrongMethodTypeException: handle's method type (int)int but found (int)long
//   fact bound-static-box-type (int)int java.lang.String
// Run (both modes): cratonvm [--compatible] J27MhRows ; java J27MhRows
// Facts are deterministic; timing lines (`time <row> <ns/op>`) only with -Dj27.time=true.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class J27MhRows {
    static class Acc {
        final int base;

        Acc(int base) {
            this.base = base;
        }

        int plus(int x) {
            return base + x;
        }
    }

    static long addL(Long a, int b) {
        return a + b;
    }

    static int addS(Short a, int b) {
        return a + b;
    }

    static int addC(Character a, int b) {
        return a + b;
    }

    static int pickZ(Boolean a, int b) {
        return a ? b : -b;
    }

    static String show(Object o, int i) {
        return o + ":" + i;
    }

    static int boom(Short s, int i) {
        if (i == -1) {
            throw new IllegalStateException("boom " + s);
        }
        return i;
    }

    static int twiceI(int x) {
        return 2 * x;
    }

    static long twiceJ(long x) {
        return 2 * x;
    }

    static short negS(short x) {
        return (short) -x;
    }

    static char nextC(char c) {
        return (char) (c + 1);
    }

    static boolean notZ(boolean z) {
        return !z;
    }

    // ---- j27 wave 6 (lane T1LAM2, CRATONVM_JIT_MH_CONSTANT_COMPILE_BOUND_STATIC) ----

    /** A mutable bound value: the compiled call must read the live object, not a copy. */
    static final class Box {
        int v;
    }

    static int readBox(Box b, int k) {
        return b.v + k;
    }

    static int boomBox(Box b, int k) {
        if (k == -1) {
            throw new IllegalStateException("boomBox " + b.v);
        }
        return b.v ^ k;
    }

    static boolean lazyInited;

    /** findStatic does not initialise the class; the first invokeExact does. */
    static final class Lazy {
        static {
            lazyInited = true;
        }

        static int get(Box b, int k) {
            return b.v * 3 + k;
        }
    }

    static final int N = 100_000;
    static final int N_BIND_EACH = 20_000;
    static final int REPS = 6;
    static final boolean TIME = Boolean.getBoolean("j27.time");

    static final MethodHandle ADDL, ADDS, ADDC, PICKZ, SHOW, BOOM, TWICEI, TWICEJ, PLUS;
    static final MethodHandle BL, BL41, BS, BC, BZ, BZF, BNULL, BSTR, BBOOM, CB;
    static final MethodHandle TJ, TS, TC, TZ;
    // j27 wave 5 (lane MHASTYPE): twiceI.asType((Number)Integer) and
    // Acc.plus.asType((Acc,Object)Object).
    static final MethodHandle TI_NUM, TPLUS;
    static final MethodHandle[] BL_PAIR;
    static final Acc ACC = new Acc(3);
    // j27 wave 6 (lane T1LAM2): bound statics over a mutable Box.
    static final Box BOX = new Box();
    static final MethodHandle BREAD, BBOOMBOX, BLAZY;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            ADDL = l.findStatic(J27MhRows.class, "addL",
                    MethodType.methodType(long.class, Long.class, int.class));
            ADDS = l.findStatic(J27MhRows.class, "addS",
                    MethodType.methodType(int.class, Short.class, int.class));
            ADDC = l.findStatic(J27MhRows.class, "addC",
                    MethodType.methodType(int.class, Character.class, int.class));
            PICKZ = l.findStatic(J27MhRows.class, "pickZ",
                    MethodType.methodType(int.class, Boolean.class, int.class));
            SHOW = l.findStatic(J27MhRows.class, "show",
                    MethodType.methodType(String.class, Object.class, int.class));
            BOOM = l.findStatic(J27MhRows.class, "boom",
                    MethodType.methodType(int.class, Short.class, int.class));
            TWICEI = l.findStatic(J27MhRows.class, "twiceI",
                    MethodType.methodType(int.class, int.class));
            TWICEJ = l.findStatic(J27MhRows.class, "twiceJ",
                    MethodType.methodType(long.class, long.class));
            PLUS = l.findVirtual(Acc.class, "plus", MethodType.methodType(int.class, int.class));
            BL = ADDL.bindTo(Long.valueOf(40));
            BL41 = ADDL.bindTo(Long.valueOf(41));
            BS = ADDS.bindTo(Short.valueOf((short) 3));
            BC = ADDC.bindTo(Character.valueOf('A'));
            BZ = PICKZ.bindTo(Boolean.TRUE);
            BZF = PICKZ.bindTo(Boolean.FALSE);
            BNULL = SHOW.bindTo(null);
            BSTR = SHOW.bindTo("s");
            BBOOM = BOOM.bindTo(Short.valueOf((short) 3));
            CB = PLUS.bindTo(new Acc(5));
            MethodType oo = MethodType.methodType(Object.class, Object.class);
            TJ = TWICEJ.asType(oo);
            TS = l.findStatic(J27MhRows.class, "negS",
                    MethodType.methodType(short.class, short.class)).asType(oo);
            TC = l.findStatic(J27MhRows.class, "nextC",
                    MethodType.methodType(char.class, char.class)).asType(oo);
            TZ = l.findStatic(J27MhRows.class, "notZ",
                    MethodType.methodType(boolean.class, boolean.class)).asType(oo);
            TI_NUM = TWICEI.asType(MethodType.methodType(Integer.class, Number.class));
            TPLUS = PLUS.asType(MethodType.methodType(Object.class, Acc.class, Object.class));
            BL_PAIR = new MethodHandle[] {BL, BL41};
            MethodType boxInt = MethodType.methodType(int.class, Box.class, int.class);
            BREAD = l.findStatic(J27MhRows.class, "readBox", boxInt).bindTo(BOX);
            BBOOMBOX = l.findStatic(J27MhRows.class, "boomBox", boxInt).bindTo(BOX);
            BLAZY = l.findStatic(Lazy.class, "get", boxInt).bindTo(BOX);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    // ---- kernels: each sums N calls of one constant (or alternating) handle ----

    static long exactStatic(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (int) TWICEI.invokeExact(i);
        }
        return s;
    }

    static long unboundVirtual(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (int) PLUS.invokeExact(ACC, i);
        }
        return s;
    }

    static long boundVirtual(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (int) CB.invokeExact(i);
        }
        return s;
    }

    static long boundStaticLong(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (long) BL.invokeExact(i);
        }
        return s;
    }

    static long boundStaticShort(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (int) BS.invokeExact(i);
        }
        return s;
    }

    static long boundStaticChar(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (int) BC.invokeExact(i);
        }
        return s;
    }

    static long boundStaticBoolean(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (int) BZ.invokeExact(i);
        }
        return s;
    }

    static long boundStaticNull(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += ((String) BNULL.invokeExact(i)).length();
        }
        return s;
    }

    static long boundStaticString(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += ((String) BSTR.invokeExact(i)).length();
        }
        return s;
    }

    static long boundStaticPair(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            MethodHandle h = BL_PAIR[i & 1];
            s += (long) h.invokeExact(i);
        }
        return s;
    }

    static long bindEachStatic(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            MethodHandle h = ADDL.bindTo(Long.valueOf(i & 7));
            s += (long) h.invokeExact(i);
        }
        return s;
    }

    // ---- j27 wave 6 (lane T1LAM2): bound statics compiled as invokestatic(argL1, a*) ----

    /** BOX.v changes every call: a call that folded the bound object's field would differ. */
    static long boundStaticLive(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            BOX.v = i & 1023;
            s += (int) BREAD.invokeExact(i & 7);
        }
        BOX.v = 0;
        return s;
    }

    /** Every 1000th call throws inside the bound static: count, message, top two frames. */
    static long boundStaticThrowHot(int n) throws Throwable {
        long s = 0;
        long thrown = 0;
        long badFrames = 0;
        for (int i = 0; i < n; i++) {
            int k = i % 1000 == 999 ? -1 : i & 255;
            try {
                s += (int) BBOOMBOX.invokeExact(k);
            } catch (IllegalStateException e) {
                thrown++;
                StackTraceElement[] st = e.getStackTrace();
                if (!"boomBox".equals(st[0].getMethodName())
                        || !"boundStaticThrowHot".equals(st[1].getMethodName())
                        || !("boomBox " + BOX.v).equals(e.getMessage())) {
                    badFrames++;
                }
            }
        }
        return s * 1_000_000L + thrown * 1000L + badFrames;
    }

    static long boundStaticLazy(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            BOX.v = i & 15;
            s += (int) BLAZY.invokeExact(i & 3);
        }
        BOX.v = 0;
        return s;
    }

    static Object convBox(int i) {
        int v = i & 127;
        switch (i & 3) {
            case 0:
                return Long.valueOf(v);
            case 1:
                return Integer.valueOf(v);
            case 2:
                return Short.valueOf((short) v);
            default:
                return Character.valueOf((char) v);
        }
    }

    static long convMixed(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            Object r = (Object) TJ.invokeExact(convBox(i));
            s += (Long) r;
        }
        return s;
    }

    // ---- j27 wave 5 (lane MHASTYPE): real asType results, hot ----

    static Object convBoxI(int i) {
        int v = i & 127;
        switch (i & 3) {
            case 0:
                return Integer.valueOf(v);
            case 1:
                return Short.valueOf((short) v);
            case 2:
                return Byte.valueOf((byte) v);
            default:
                return Character.valueOf((char) v);
        }
    }

    /** Every 1000 calls: a Float and a Boolean (CCE), then a null (NPE). */
    static Object faultArg(int i) {
        int k = i % 1000;
        if (k == 997) {
            return Float.valueOf(1.5f);
        }
        if (k == 998) {
            return Boolean.TRUE;
        }
        if (k == 999) {
            return null;
        }
        return convBox(i);
    }

    /** sum * 10^6 + cce * 1000 + npe, or -(wrong CCE messages). */
    static long convFaults(int n) throws Throwable {
        long s = 0;
        int cce = 0;
        int npe = 0;
        int bad = 0;
        for (int i = 0; i < n; i++) {
            try {
                Object r = (Object) TJ.invokeExact(faultArg(i));
                s += (Long) r;
            } catch (ClassCastException e) {
                cce++;
                String want = i % 1000 == 997
                        ? "Cannot cast java.lang.Float to java.lang.Long"
                        : "Cannot cast java.lang.Boolean to java.lang.Long";
                if (!want.equals(e.getMessage())) {
                    bad++;
                }
            } catch (NullPointerException e) {
                npe++;
            }
        }
        return bad != 0 ? -bad : s * 1_000_000L + cce * 1000L + npe;
    }

    /** cached * 10^6 + fresh: Long.valueOf's cache identity of the boxed result. */
    static long convIdentity(int n) throws Throwable {
        long cached = 0;
        long fresh = 0;
        for (int i = 0; i < n; i++) {
            long v = (i & 255) - 128;
            Object r = (Object) TJ.invokeExact((Object) Long.valueOf(v));
            if (r == Long.valueOf(2 * v)) {
                cached++;
            } else {
                fresh++;
            }
        }
        return cached * 1_000_000L + fresh;
    }

    /** Results equal to Long.MIN_VALUE (every even call). */
    static long convMin(int n) throws Throwable {
        long s = 0;
        Object half = Long.valueOf(Long.MIN_VALUE / 2);
        for (int i = 0; i < n; i++) {
            Object a = (i & 1) == 0 ? half : convBox(i);
            Object r = (Object) TJ.invokeExact(a);
            if ((Long) r == Long.MIN_VALUE) {
                s++;
            }
        }
        return s;
    }

    static long convNumber(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            int v = i & 127;
            int k = i % 3;
            Number a = k == 0 ? (Number) Integer.valueOf(v)
                    : k == 1 ? (Number) Short.valueOf((short) v) : (Number) Byte.valueOf((byte) v);
            Integer r = (Integer) TI_NUM.invokeExact(a);
            s += r;
        }
        return s;
    }

    static long convVirtual(int n) throws Throwable {
        long s = 0;
        for (int i = 0; i < n; i++) {
            Object r = (Object) TPLUS.invokeExact(ACC, convBoxI(i));
            s += (Integer) r;
        }
        return s;
    }

    interface Kernel {
        long run(int n) throws Throwable;
    }

    static void row(String name, Kernel k, int n) throws Throwable {
        long first = k.run(n);
        boolean same = true;
        long t0 = System.nanoTime();
        for (int r = 1; r < REPS; r++) {
            same &= k.run(n) == first;
        }
        long t1 = System.nanoTime();
        System.out.println("fact " + name + " " + first + " same=" + same);
        if (TIME) {
            System.out.println("time " + name + " " + (t1 - t0) / ((long) (REPS - 1) * n));
        }
    }

    // ---- single calls ----

    static String describe(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    interface Call {
        Object call() throws Throwable;
    }

    /** The result and its class, or the exception and its message. */
    static void one(String name, Call c) {
        String out;
        try {
            Object r = c.call();
            out = r + " " + (r == null ? "null" : r.getClass().getName());
        } catch (Throwable t) {
            out = describe(t);
        }
        System.out.println("fact " + name + " " + out);
    }

    /** Only the exception's class (NPE messages from JDK-internal code are not compared). */
    static void oneClass(String name, Call c) {
        String out;
        try {
            out = "no-throw " + c.call();
        } catch (Throwable t) {
            out = t.getClass().getName();
        }
        System.out.println("fact " + name + " " + out);
    }

    public static void main(String[] args) throws Throwable {
        // j27 wave 6 (lane T1LAM2): the bound static's class is initialised by
        // its first invokeExact, not by findStatic / bindTo. FIRST in main.
        one("bound-static-clinit", () -> {
            boolean before = lazyInited;
            int r = (int) BLAZY.invokeExact(1);
            return before + " " + lazyInited + " " + r;
        });
        // Constant-handle compiles (MH7-4) and the bound statics (MH16-7).
        row("exact-static-sum", J27MhRows::exactStatic, N);
        row("unbound-virtual-sum", J27MhRows::unboundVirtual, N);
        row("bound-virtual-sum", J27MhRows::boundVirtual, N);
        row("bound-static-long-sum", J27MhRows::boundStaticLong, N);
        row("bound-static-short-sum", J27MhRows::boundStaticShort, N);
        row("bound-static-char-sum", J27MhRows::boundStaticChar, N);
        row("bound-static-boolean-sum", J27MhRows::boundStaticBoolean, N);
        row("bound-static-null-len", J27MhRows::boundStaticNull, N);
        row("bound-static-string-len", J27MhRows::boundStaticString, N);
        row("bound-static-pair-sum", J27MhRows::boundStaticPair, N);
        row("bind-each-static-sum", J27MhRows::bindEachStatic, N_BIND_EACH);
        // j27 wave 6 (lane T1LAM2).
        row("bound-static-live-sum", J27MhRows::boundStaticLive, N);
        row("bound-static-throw-hot", J27MhRows::boundStaticThrowHot, N);
        row("bound-static-lazy-sum", J27MhRows::boundStaticLazy, N);
        row("conv-mixed-sum", J27MhRows::convMixed, N);
        // j27 wave 5 (lane MHASTYPE).
        row("conv-faults", J27MhRows::convFaults, N);
        row("conv-identity", J27MhRows::convIdentity, N);
        row("conv-min-value-count", J27MhRows::convMin, N);
        row("conv-number-sum", J27MhRows::convNumber, N);
        row("conv-virtual-sum", J27MhRows::convVirtual, N);

        // Bound statics, one call each (after the hot loops).
        one("bound-static-long", () -> (long) BL.invokeExact(2));
        one("bound-static-short", () -> (int) BS.invokeExact(2));
        one("bound-static-char", () -> (int) BC.invokeExact(1));
        one("bound-static-true", () -> (int) BZ.invokeExact(7));
        one("bound-static-false", () -> (int) BZF.invokeExact(7));
        one("bound-static-null", () -> (String) BNULL.invokeExact(5));
        one("bound-static-string", () -> (String) BSTR.invokeExact(5));
        one("bound-static-type", () -> BL.type().toString());
        one("bound-static-wmte", () -> (int) BL.invokeExact(2));
        one("bound-static-throw", () -> {
            try {
                return (int) BBOOM.invokeExact(-1);
            } catch (IllegalStateException e) {
                return describe(e) + " top=" + e.getStackTrace()[0].getMethodName();
            }
        });
        // j27 wave 6 (lane T1LAM2), after the hot loops.
        one("bound-static-box-wmte", () -> (long) BREAD.invokeExact(1));
        one("bound-static-box-type", () -> BREAD.type().toString());
        one("bind-to-long-with-integer", () -> ADDL.bindTo(Integer.valueOf(1)));
        one("bind-to-long-with-short", () -> ADDL.bindTo(Short.valueOf((short) 1)));
        one("bind-to-char-with-integer", () -> ADDC.bindTo(Integer.valueOf(65)));
        one("bind-to-boolean-with-string", () -> PICKZ.bindTo("true"));
        one("bind-to-primitive-leading", () -> TWICEJ.bindTo(5L));
        oneClass("bound-virtual-null-receiver", () -> (int) PLUS.bindTo(null).invokeExact(1));

        // Converting handles over long / short / char / boolean members (MH6-1's shape).
        one("conv-long-Long", () -> (Object) TJ.invokeExact((Object) Long.valueOf(5)));
        one("conv-long-Integer", () -> (Object) TJ.invokeExact((Object) Integer.valueOf(7)));
        one("conv-long-Short", () -> (Object) TJ.invokeExact((Object) Short.valueOf((short) 3)));
        one("conv-long-Byte", () -> (Object) TJ.invokeExact((Object) Byte.valueOf((byte) -2)));
        one("conv-long-Character", () -> (Object) TJ.invokeExact((Object) Character.valueOf('A')));
        one("conv-long-Boolean", () -> (Object) TJ.invokeExact((Object) Boolean.TRUE));
        oneClass("conv-long-null", () -> (Object) TJ.invokeExact((Object) null));
        one("conv-short-Short", () -> (Object) TS.invokeExact((Object) Short.valueOf((short) 4)));
        one("conv-short-Byte", () -> (Object) TS.invokeExact((Object) Byte.valueOf((byte) 5)));
        one("conv-short-Integer", () -> (Object) TS.invokeExact((Object) Integer.valueOf(1)));
        one("conv-short-Character", () -> (Object) TS.invokeExact((Object) Character.valueOf('A')));
        one("conv-short-Boolean", () -> (Object) TS.invokeExact((Object) Boolean.TRUE));
        oneClass("conv-short-null", () -> (Object) TS.invokeExact((Object) null));
        one("conv-char-Character", () -> (Object) TC.invokeExact((Object) Character.valueOf('A')));
        one("conv-char-Integer", () -> (Object) TC.invokeExact((Object) Integer.valueOf(66)));
        one("conv-char-Short", () -> (Object) TC.invokeExact((Object) Short.valueOf((short) 66)));
        oneClass("conv-char-null", () -> (Object) TC.invokeExact((Object) null));
        one("conv-boolean-Boolean", () -> (Object) TZ.invokeExact((Object) Boolean.TRUE));
        one("conv-boolean-Integer", () -> (Object) TZ.invokeExact((Object) Integer.valueOf(1)));
        oneClass("conv-boolean-null", () -> (Object) TZ.invokeExact((Object) null));
        one("conv-invoke-Short", () -> (long) TWICEJ.invoke((Object) Short.valueOf((short) 9)));
        one("conv-invoke-Boolean", () -> (long) TWICEJ.invoke((Object) Boolean.TRUE));
        one("conv-wmte", () -> (Long) TJ.invokeExact((Object) Long.valueOf(1)));
        one("exact-wmte", () -> (long) TWICEI.invokeExact(1));
        // j27 wave 5 (lane MHASTYPE), after the hot loops.
        one("conv-number-Short", () -> (Integer) TI_NUM.invokeExact((Number) Short.valueOf((short) -4)));
        one("conv-number-Long", () -> (Integer) TI_NUM.invokeExact((Number) Long.valueOf(1)));
        one("conv-number-Double", () -> (Integer) TI_NUM.invokeExact((Number) Double.valueOf(1)));
        oneClass("conv-number-null", () -> (Integer) TI_NUM.invokeExact((Number) null));
        one("conv-virtual-Character", () -> (Object) TPLUS.invokeExact(ACC, (Object) Character.valueOf('A')));
        one("conv-virtual-Long", () -> (Object) TPLUS.invokeExact(ACC, (Object) Long.valueOf(1)));
        oneClass("conv-virtual-null-receiver", () -> (Object) TPLUS.invokeExact((Acc) null, (Object) Integer.valueOf(1)));
        one("conv-min-value", () -> (Object) TJ.invokeExact((Object) Long.valueOf(Long.MIN_VALUE / 2)));
        one("conv-box-cached", () -> (Object) TJ.invokeExact((Object) Long.valueOf(3)) == Long.valueOf(6));
        one("conv-box-fresh", () -> (Object) TJ.invokeExact((Object) Long.valueOf(100)) == Long.valueOf(200));
        one("conv-number-wmte", () -> (Object) TI_NUM.invokeExact((Number) Integer.valueOf(1)));
    }
}
