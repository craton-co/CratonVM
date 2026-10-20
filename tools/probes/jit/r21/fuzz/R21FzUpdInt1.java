// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: the `Unsafe` instance-field arm under byte
// field offsets (`CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS`,
// `CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL`,
// `CRATONVM_JIT_UNSAFE_RMW_VETO_NEEDS_FIELD_ARM`,
// `CRATONVM_JIT_IR_SPLICE_NEVER_RUN_PRIVATE_EXIT`), driven through
// AtomicIntegerFieldUpdater / AtomicLongFieldUpdater.
//
// Risk targeted: the cell table is keyed `(class id, slot << 1 | wide)` and the
// screen decodes `16 + slot * 8`. A cell published for one class must never
// serve another class's object at the same offset, and a subclass object
// (another class id) must be served by its own cell or the natives:
//   * one updater on a field DECLARED in Base, used with Base, Sub and Sub2
//     objects (three class ids, one offset);
//   * two classes with a same-named, same-position field and their own
//     updaters, interleaved (same slot, different class ids);
//   * int and long fields at neighbouring slots (the wide bit);
//   * every op: get / set / lazySet / compareAndSet (success and failure) /
//     getAndAdd / getAndIncrement / getAndSet / addAndGet / updateAndGet /
//     accumulateAndGet;
//   * the accessCheck failure (an object of the wrong class: ClassCastException)
//     and a null object, at a site that was hot with good objects (the
//     never-run private `throwAccessCheckException` exit);
//   * two threads hammering getAndIncrement on one field: the final count is
//     exact.
//
// Run:
//   javac -d out R21FzUpdInt1.java
//   java -Xmx256m -cp out R21FzUpdInt1                                    # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzUpdInt1
//   CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS=1 CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL=1 cratonvm ...
//   (that arm) + CRATONVM_JIT_UNSAFE_RMW_VETO_NEEDS_FIELD_ARM=1 + CRATONVM_JIT_IR_SPLICE_NEVER_RUN_PRIVATE_EXIT=1
//   CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS=0 cratonvm ... (the slot-offset control)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.concurrent.atomic.AtomicLongFieldUpdater;

public class R21FzUpdInt1 {
    static final int WARM = 60_000;

    static class Base {
        volatile int a;
        volatile long b;
        volatile int c;
    }

    static class Sub extends Base {
        volatile int d;
    }

    static final class Sub2 extends Sub {
        volatile long e;
    }

    static final class Twin {
        volatile int a;
        volatile long b;
        volatile int c;
    }

    static final AtomicIntegerFieldUpdater<Base> A = AtomicIntegerFieldUpdater.newUpdater(Base.class, "a");
    static final AtomicLongFieldUpdater<Base> B = AtomicLongFieldUpdater.newUpdater(Base.class, "b");
    static final AtomicIntegerFieldUpdater<Base> C = AtomicIntegerFieldUpdater.newUpdater(Base.class, "c");
    static final AtomicIntegerFieldUpdater<Sub> D = AtomicIntegerFieldUpdater.newUpdater(Sub.class, "d");
    static final AtomicLongFieldUpdater<Sub2> E = AtomicLongFieldUpdater.newUpdater(Sub2.class, "e");
    static final AtomicIntegerFieldUpdater<Twin> TA = AtomicIntegerFieldUpdater.newUpdater(Twin.class, "a");
    static final AtomicLongFieldUpdater<Twin> TB = AtomicLongFieldUpdater.newUpdater(Twin.class, "b");

    static long ops(Base o, Twin t, int r) {
        long h = 0;
        h = h * 31 + A.getAndAdd(o, r);
        h = h * 31 + A.get(o);
        A.set(o, A.get(o) ^ 0x55);
        h = h * 31 + (A.compareAndSet(o, A.get(o), r) ? 1 : 0);
        h = h * 31 + (A.compareAndSet(o, r + 1, 0) ? 1 : 0);
        h = h * 31 + A.getAndIncrement(o);
        h = h * 31 + A.getAndSet(o, -r);
        h = h * 31 + A.addAndGet(o, 3);
        h = h * 31 + A.updateAndGet(o, x -> x * 3);
        h = h * 31 + A.accumulateAndGet(o, 7, Integer::sum);
        A.lazySet(o, A.get(o) + 1);
        h = h * 31 + B.getAndAdd(o, (long) r << 33);
        h = h * 31 + B.incrementAndGet(o);
        h = h * 31 + (B.compareAndSet(o, B.get(o), -1L) ? 1 : 0);
        B.set(o, B.get(o) + 0x1_0000_0001L);
        h = h * 31 + C.incrementAndGet(o);
        C.set(o, C.get(o) - 2);
        if (o instanceof Sub s) {
            h = h * 31 + D.getAndAdd(s, 5);
        }
        if (o instanceof Sub2 s2) {
            h = h * 31 + E.getAndAdd(s2, 1L << 40);
        }
        h = h * 31 + TA.getAndAdd(t, 2);
        h = h * 31 + TB.addAndGet(t, -3L);
        TA.set(t, TA.get(t) + 1);
        return h;
    }

    static String state(Base o) {
        String s = o.a + "," + o.b + "," + o.c;
        if (o instanceof Sub sub) {
            s += "," + sub.d;
        }
        if (o instanceof Sub2 s2) {
            s += "," + s2.e;
        }
        return s;
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static String misuse(Object wrong) {
        AtomicIntegerFieldUpdater raw = A;
        try {
            return "got " + raw.get(wrong);
        } catch (RuntimeException e) {
            return e.getClass().getName() + ":" + e.getMessage();
        }
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static String misuseCas(Object wrong) {
        AtomicLongFieldUpdater raw = B;
        try {
            return "cas " + raw.compareAndSet(wrong, 0L, 1L);
        } catch (RuntimeException e) {
            return e.getClass().getName() + ":" + e.getMessage();
        }
    }

    /** One hot raw-updater site, warmed with good objects, later fed a wrong one. */
    @SuppressWarnings({"unchecked", "rawtypes"})
    static int hotRaw(Object o) {
        AtomicIntegerFieldUpdater raw = A;
        return raw.getAndAdd(o, 1) + raw.get(o);
    }

    static String hotRawRow(Object o) {
        try {
            return "ok " + hotRaw(o);
        } catch (RuntimeException e) {
            return e.getClass().getName() + ":" + e.getMessage();
        }
    }

    static void rows(String tag) {
        Base[] objs = {new Base(), new Sub(), new Sub2()};
        Twin t = new Twin();
        StringBuilder sb = new StringBuilder("row " + tag);
        for (int i = 0; i < objs.length; i++) {
            long h = 0;
            for (int r = 0; r < 5; r++) {
                h = h * 31 + ops(objs[i], t, r + i);
            }
            sb.append(" | ").append(h).append(' ').append(state(objs[i]));
        }
        sb.append(" | twin ").append(t.a).append(',').append(t.b).append(',').append(t.c);
        System.out.println(sb);
        System.out.println("row " + tag + " misuse " + misuse(new Twin()) + " " + misuse(null) + " "
                + misuse("str") + " " + misuseCas(new Twin()) + " " + misuseCas(null));
    }

    public static void main(String[] args) throws Exception {
        rows("cold");
        Base[] objs = {new Base(), new Sub(), new Sub2()};
        Twin t = new Twin();
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + ops(objs[r % 3], t, r);
            h = h * 31 + hotRaw(objs[(r + 1) % 3]);
        }
        System.out.println("warm hash " + h + " " + state(objs[0]) + " " + state(objs[1]) + " " + state(objs[2])
                + " twin " + t.a + "," + t.b);
        rows("hot");
        System.out.println("hotRaw " + hotRawRow(new Twin()) + " | " + hotRawRow(null) + " | " + hotRawRow(new Sub2())
                + " | " + hotRawRow("x"));
        Sub shared = new Sub();
        Thread[] ts = new Thread[2];
        for (int k = 0; k < 2; k++) {
            ts[k] = new Thread(() -> {
                for (int i = 0; i < 200_000; i++) {
                    A.getAndIncrement(shared);
                    D.addAndGet(shared, 2);
                    B.getAndAdd(shared, 3L);
                }
            });
            ts[k].start();
        }
        for (Thread th : ts) {
            th.join();
        }
        System.out.println("race " + state(shared));
        rows("after");
    }
}
