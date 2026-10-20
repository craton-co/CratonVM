// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 16, lane review-fuzz5: one set of fields and one int[]
// updated through three doors in the same hot loops -- VarHandle access modes,
// sun.misc.Unsafe, and the java.util.concurrent.atomic field updaters -- with
// loop bounds that wrap at Integer.MAX_VALUE and Long.MAX_VALUE.
//
// Risk targeted: the VarHandle fold and the unsafe field-cell arms
// (`CRATONVM_JIT_IR_VH_*`, `CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE`,
// `..._CAE_INLINE`, `..._BITWISE_INLINE`, `..._FIELD_CELLS_BYTE_OFFSETS`) when
// two doors meet on one field: a cached load of one door's store, a wrong
// width (int RMW on the low half of a long, a long store clobbering the
// neighbouring int), or a stale value after a deopt. Also the IR tier's
// counted-loop recognition on bounds that wrap: `for (int i = MAX - 5000;
// i >= MAX - 5000; i += 7)` ends only when `i` wraps (715 trips), and the
// long twin wraps through Long.MIN_VALUE. Every fact is a pure function of the
// operations, so each pass's hash is deterministic.
//
// sun.misc.Unsafe: JDK 25 prints a one-time deprecation WARNING on stderr at
// the first memory-access call. Diff STDOUT only. Offsets and array scales are
// used, never printed (they are layout facts).
//
// Passes: `cold`, `hot0..2` (the kernel 1500 times (783 inner trips each), plus one long OSR call),
// `late` (a never-loaded `Holder` subclass becomes the receiver: the receiver
// type checks of the VarHandles and the updaters, a deopt), `rehot0..1`,
// `errors` (wrong receiver class through the VarHandle and the updater, a
// null receiver: class names only).
//
// Run:
//   javac -d out R21Fz5VhUnsafeMix1.java
//   java -Xmx256m -cp out R21Fz5VhUnsafeMix1 2>/dev/null           # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz5VhUnsafeMix1 2>/dev/null       # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz5VhUnsafeMix1 2>/dev/null
//   cratonvm --nojit -Xmx256m -cp out R21Fz5VhUnsafeMix1 2>/dev/null
//   CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE=0 cratonvm ...                  # control arm
// HotSpot expectation: every line identical on every arm; `hot0..2` equal,
// `rehot0..1` equal.

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Field;
import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.concurrent.atomic.AtomicLongFieldUpdater;

public class R21Fz5VhUnsafeMix1 {
    static final sun.misc.Unsafe U;
    static final long OFF_VI;
    static final long OFF_L;
    static final long OFF_B;
    static final long OFF_S;
    static final long ARR_BASE;
    static final long ARR_SCALE;
    static final VarHandle VI;
    static final VarHandle VL;
    static final VarHandle VB;
    static final VarHandle VS;
    static final VarHandle VARR;
    static final AtomicIntegerFieldUpdater<Holder> UPD_I =
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, "vi");
    static final AtomicLongFieldUpdater<Holder> UPD_L =
            AtomicLongFieldUpdater.newUpdater(Holder.class, "vl");

    static {
        try {
            Field f = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
            f.setAccessible(true);
            U = (sun.misc.Unsafe) f.get(null);
            OFF_VI = U.objectFieldOffset(Holder.class.getDeclaredField("vi"));
            OFF_L = U.objectFieldOffset(Holder.class.getDeclaredField("vl"));
            OFF_B = U.objectFieldOffset(Holder.class.getDeclaredField("b"));
            OFF_S = U.objectFieldOffset(Holder.class.getDeclaredField("s"));
            ARR_BASE = U.arrayBaseOffset(int[].class);
            ARR_SCALE = U.arrayIndexScale(int[].class);
            MethodHandles.Lookup lk = MethodHandles.lookup();
            VI = lk.findVarHandle(Holder.class, "vi", int.class);
            VL = lk.findVarHandle(Holder.class, "vl", long.class);
            VB = lk.findVarHandle(Holder.class, "b", byte.class);
            VS = lk.findVarHandle(Holder.class, "s", short.class);
            VARR = MethodHandles.arrayElementVarHandle(int[].class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static class Holder {
        volatile int vi;
        byte b;
        short s;
        volatile long vl;
        int tail;
        final int[] arr = new int[16];
    }

    static long mix(long h, long v) {
        return (h ^ v) * 0x100000001B3L;
    }

    static long dump(Holder h) {
        long x = mix(mix(mix(mix(17, h.vi), h.b), h.s), h.vl);
        x = mix(x, h.tail);
        for (int v : h.arr) {
            x = mix(x, v);
        }
        return x;
    }

    // One kernel trip: every door on the same fields.
    static long kernel(Holder h, int seed) {
        long r = 0;
        int trips = 0;
        // int bound that wraps: 715 trips.
        for (int i = Integer.MAX_VALUE - 5000; i >= Integer.MAX_VALUE - 5000; i += 7) {
            int k = i & 15;
            switch (k & 3) {
                case 0 -> r += (int) VI.getAndAdd(h, seed);
                case 1 -> r += U.getAndAddInt(h, OFF_VI, -seed >> 1);
                case 2 -> r += UPD_I.addAndGet(h, k);
                default -> r += U.compareAndSwapInt(h, OFF_VI, h.vi, h.vi ^ k) ? 1 : 0;
            }
            int old = (int) VARR.getAndBitwiseXor(h.arr, k, i);
            int seen = U.getIntVolatile(h.arr, ARR_BASE + (long) k * ARR_SCALE);
            r = mix(r, old ^ seen);
            VB.getAndAdd(h, (byte) 100);
            U.putShort(h, OFF_S, (short) (U.getShort(h, OFF_S) + 30000));
            h.tail += (byte) VB.get(h);
            trips++;
        }
        // long bound that wraps through Long.MIN_VALUE: 64 + 4 trips.
        for (long j = Long.MAX_VALUE - 63; j != Long.MIN_VALUE + 4; j++) {
            long add = (j & 1) == 0 ? Long.MAX_VALUE / 3 : j;
            long before = (long) VL.getAndAdd(h, add);
            long cas = U.getLongVolatile(h, OFF_L);
            if (!U.compareAndSwapLong(h, OFF_L, cas, cas + seed)) {
                r++;
            }
            r = mix(r, before);
            UPD_L.getAndAdd(h, -1L);
            trips++;
        }
        short sv = (short) VS.getAndBitwiseOr(h, (short) 0x0101);
        VS.setRelease(h, (short) (sv + 1));
        return mix(r, trips);
    }

    static long run(Holder h, int reps) {
        long acc = 0;
        for (int r = 0; r < reps; r++) {
            acc = mix(acc, kernel(h, r & 63));
        }
        return mix(acc, dump(h));
    }

    // One long call: the same doors in an OSR-compiled loop.
    static long osr(Holder h, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += (int) VI.getAndAdd(h, 1);
            acc += U.getAndAddInt(h, OFF_VI, -1);
            acc ^= (long) VL.getAndBitwiseXor(h, (long) i << 33);
            int k = i & 15;
            U.putIntVolatile(h.arr, ARR_BASE + (long) k * ARR_SCALE, (int) VARR.getVolatile(h.arr, (k + 1) & 15) + i);
        }
        return mix(acc, dump(h));
    }

    static void pass(String tag, Holder h, int reps) {
        long a = run(h, reps);
        long o = osr(h, reps * 200);
        System.out.println(tag + " run=" + a + " osr=" + o);
    }

    static String err(Runnable r) {
        try {
            r.run();
            return "ok";
        } catch (RuntimeException e) {
            return e.getClass().getName();
        }
    }

    public static void main(String[] args) throws Exception {
        pass("cold", new Holder(), 3);
        for (int r = 0; r < 3; r++) {
            pass("hot" + r, new Holder(), 1500);
        }
        Holder late = (Holder) Class.forName("R21Fz5VhUnsafeMix1$Late").getDeclaredConstructor().newInstance();
        pass("late", late, 1500);
        for (int r = 0; r < 2; r++) {
            pass("rehot" + r, new Holder(), 1500);
        }
        Object wrong = "not a holder";
        System.out.println("errors vh=" + err(() -> VI.getAndAdd(wrong, 1))
                + " upd=" + err(() -> {
                    @SuppressWarnings({"unchecked", "rawtypes"})
                    AtomicIntegerFieldUpdater raw = UPD_I;
                    raw.addAndGet(wrong, 1);
                })
                + " vhNull=" + err(() -> VI.getAndAdd((Holder) null, 1))
                + " arrIdx=" + err(() -> VARR.getAndAdd(new int[4], 4, 1)));
        System.out.println("done");
    }

    static class Late extends Holder {
        int extra = 5;
    }
}
