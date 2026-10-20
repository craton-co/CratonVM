// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: EA21-1 (`CRATONVM_JIT_EA_REF_COMPARE_FOLD`)
// against objects that are NOT fresh although the bytecode looks like an
// allocation site, and against holders reached through intrinsics.
//
// Risk targeted, two halves:
//  * box / string identity. `Integer.valueOf(x)` returns the CACHED box for
//    -128..127 (Byte / Short / Long / Character <= 127 / Boolean always), so
//    `Integer a = x, b = x; a == b` is TRUE there and false outside it. If
//    the box fold (round 9) or a valueOf intrinsic hands escape analysis an
//    `Op::New` for a box that is really the cache entry, the fold answers 0
//    in the cached range. Same for `intern()` and string literals, and enum
//    `valueOf`.
//  * holders the bridge maps through something other than a plain field
//    edge: VarHandle / AtomicReferenceFieldUpdater / AtomicReference set +
//    get / CAS, reflective Field.get, MethodHandles.identity, ThreadLocal,
//    List.of / Arrays.asList / Map.of, Throwable cause / suppressed. Each
//    reads back the fresh object; a fold that thinks the read cannot be it
//    prints 0.
// Cold, hot (200k trips), after a deopt trigger (a new box range and a new
// receiver class), again.
//
// Run:
//   javac -d out R21Fz2EaCmp2.java          (a removal warning for new Integer(int) is expected)
//   java -Xmx256m -cp out R21Fz2EaCmp2                                   # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2EaCmp2
//   CRATONVM_JIT_EA_REF_COMPARE_FOLD=1 cratonvm -Xmx256m -cp out R21Fz2EaCmp2
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm (no -XX:AutoBoxCacheMax on either VM).

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Field;
import java.util.Arrays;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;

public class R21Fz2EaCmp2 {
    static final int WARM = 200_000;

    enum E { A, B, C }

    static final class H {
        volatile Object f;
        Object g;
    }

    static final class P {
        final int v;

        P(int v) {
            this.v = v;
        }
    }

    static final VarHandle VH;
    static final AtomicReferenceFieldUpdater<H, Object> UP =
            AtomicReferenceFieldUpdater.newUpdater(H.class, Object.class, "f");
    static final MethodHandle ID = MethodHandles.identity(Object.class);
    static final Field G;
    static final ThreadLocal<Object> TL = new ThreadLocal<>();

    static {
        try {
            VH = MethodHandles.lookup().findVarHandle(H.class, "f", Object.class);
            G = H.class.getDeclaredField("g");
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    // ---- boxes and strings ----
    static int b0(int x) {
        Integer a = x;
        Integer b = x;
        return (a == b ? 1 : 0) + (Integer.valueOf(x) == Integer.valueOf(x) ? 2 : 0);
    }

    @SuppressWarnings("removal")
    static int b1(int x) {
        Integer fresh = new Integer(x);
        Integer cached = Integer.valueOf(x);
        return (fresh == cached ? 1 : 0) + (fresh.equals(cached) ? 2 : 0);
    }

    static int b2(int x) {
        Long l1 = (long) x;
        Long l2 = Long.valueOf(x);
        Short s1 = (short) x;
        Short s2 = (short) x;
        Byte y1 = (byte) x;
        Byte y2 = (byte) x;
        Character c1 = (char) (x & 0xFFFF);
        Character c2 = (char) (x & 0xFFFF);
        Boolean z1 = x > 0;
        Boolean z2 = Boolean.valueOf(x > 0);
        return (l1 == l2 ? 1 : 0) + (s1 == s2 ? 2 : 0) + (y1 == y2 ? 4 : 0) + (c1 == c2 ? 8 : 0)
                + (z1 == z2 ? 16 : 0) + (z1 == Boolean.TRUE ? 32 : 0);
    }

    static int b3(int x) {
        Double d1 = (double) x;
        Double d2 = (double) x;
        Float f1 = (float) x;
        Float f2 = Float.valueOf(x);
        Integer a = x;
        Object o = a;
        Integer c = (Integer) o;
        return (d1 == d2 ? 1 : 0) + (f1 == f2 ? 2 : 0) + (c == a ? 4 : 0);
    }

    static int b4(int n) {
        Integer prev = null;
        int same = 0;
        for (int k = 0; k < n; k++) {
            Integer cur = (k * 7) % 300 - 100;
            if (prev == cur) {
                same++;
            }
            if (k % 3 == 0) {
                prev = cur;
            }
        }
        return same;
    }

    static int s0(int x) {
        String lit = "ab";
        String made = new String("ab");
        String v1 = String.valueOf(x);
        String v2 = String.valueOf(x);
        return (lit == "ab" ? 1 : 0) + (made == lit ? 2 : 0) + (made.intern() == lit ? 4 : 0) + (v1 == v2 ? 8 : 0)
                + (("k" + x).intern() == ("k" + x).intern() ? 16 : 0) + (E.valueOf("B") == E.B ? 32 : 0);
    }

    // ---- holders through intrinsics ----
    static int h0(int x) {
        H h = new H();
        P a = new P(x);
        P b = new P(x + 1);
        VH.set(h, a);
        int r = VH.get(h) == a ? 1 : 0;
        r += VH.compareAndSet(h, a, b) ? 2 : 0;
        r += h.f == b ? 4 : 0;
        r += VH.getAndSet(h, a) == b ? 8 : 0;
        r += VH.getAcquire(h) == a ? 16 : 0;
        return r;
    }

    static int h1(int x) {
        H h = new H();
        P a = new P(x);
        P b = new P(x + 1);
        UP.set(h, a);
        int r = UP.get(h) == a ? 1 : 0;
        r += UP.compareAndSet(h, b, a) ? 2 : 0;
        r += UP.compareAndSet(h, a, b) ? 4 : 0;
        r += UP.getAndSet(h, null) == b ? 8 : 0;
        r += h.f == null ? 16 : 0;
        return r;
    }

    static int h2(int x) {
        P a = new P(x);
        AtomicReference<Object> ar = new AtomicReference<>(a);
        int r = ar.get() == a ? 1 : 0;
        r += ar.compareAndSet(a, null) ? 2 : 0;
        r += ar.get() == null ? 4 : 0;
        return r;
    }

    static int h3(int x) throws Throwable {
        P a = new P(x);
        H h = new H();
        h.g = a;
        Object viaField = G.get(h);
        Object viaMh = (Object) ID.invokeExact((Object) a);
        return (viaField == a ? 1 : 0) + (viaMh == a ? 2 : 0);
    }

    static int h4(int x) {
        P a = new P(x);
        TL.set(a);
        int r = TL.get() == a ? 1 : 0;
        TL.remove();
        return r + (TL.get() == null ? 2 : 0);
    }

    static int h5(int x) {
        P a = new P(x);
        P b = new P(x);
        List<Object> l = List.of(a, b);
        List<Object> al = Arrays.asList(a, b);
        Map<Object, Object> m = Map.of(a, b);
        return (l.get(0) == a ? 1 : 0) + (al.get(1) == b ? 2 : 0) + (m.get(a) == b ? 4 : 0)
                + (m.get(b) == null ? 8 : 0) + (l.get(1) == a ? 16 : 0);
    }

    static int h6(int x) {
        RuntimeException cause = new IllegalArgumentException("c" + (x & 3));
        RuntimeException top = new RuntimeException("t", cause);
        RuntimeException sup = new UnsupportedOperationException();
        top.addSuppressed(sup);
        return (top.getCause() == cause ? 1 : 0) + (top.getSuppressed()[0] == sup ? 2 : 0)
                + (top.getCause() == sup ? 4 : 0);
    }

    static int all(int x) throws Throwable {
        return b0(x) + b1(x) + b2(x) + b3(x) + s0(x) + h0(x) + h1(x) + h2(x) + h3(x) + h4(x) + h5(x) + h6(x);
    }

    static void rows(String tag, int[] xs) throws Throwable {
        for (int x : xs) {
            System.out.println("row " + tag + " x=" + x + " | b0=" + b0(x) + " b1=" + b1(x) + " b2=" + b2(x)
                    + " b3=" + b3(x) + " b4=" + b4(x & 63) + " s0=" + s0(x) + " h0=" + h0(x) + " h1=" + h1(x)
                    + " h2=" + h2(x) + " h3=" + h3(x) + " h4=" + h4(x) + " h5=" + h5(x) + " h6=" + h6(x));
        }
    }

    public static void main(String[] args) throws Throwable {
        int[] xs = {0, 1, 5, 100, 127, 128, -1, -128, -129, 1000, 65535, 70000};
        rows("cold", xs);
        long h = 0;
        // Warm inside the cache range only, so a later out-of-range box is new
        // ground for the compiled code.
        for (int r = 0; r < WARM; r++) {
            int x = (r % 200) - 100;
            h = h * 31 + all(x) + b4(x & 31);
        }
        System.out.println("warm hash " + h);
        rows("hot", xs);
        // Deopt triggers: the box range the warm-up never saw, and a P
        // subclass-free path (E.C) through the enum switch.
        long g = 0;
        for (int r = 0; r < 2000; r++) {
            g = g * 31 + all(200 + r) + E.values()[r % 3].ordinal();
        }
        System.out.println("late hash " + g);
        rows("after", xs);
    }
}
