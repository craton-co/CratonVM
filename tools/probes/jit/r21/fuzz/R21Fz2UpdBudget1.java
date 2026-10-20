// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: the updaters' cell publication screen
// (wave 2 hand-back: `UnsafeInlineCells::field_cell_settled`, the per-VM
// `publish_budget` of 65536 failed attempts, `ref_volatile_publish_armed`,
// and `vm_exec::note_unsafe_volatile_field_cell`'s `index < 4` test for
// `java.lang.ref.Reference`'s own fields).
//
// Risk targeted: (1) a cell word non-zero for a COLLIDING owner reads
// "settled", so a field never gets its own cell -- slower, but a compiled
// arm that trusted a collided cell would read another class's field; (2)
// once the budget is spent the helpers stop publishing for every field of
// the VM -- results must not change; (3) a `Reference` subclass's OWN
// volatile fields (slots >= 4) may get a cell, while `referent` / `queue` /
// `next` / `discovered` (slots 0..3) must never be read by a compiled plain
// load. Rows: 12 holder subclasses (48 instances) x (volatile int, long, Object) through
// AIFU / ALFU / ARFU and VarHandles, every op (get / set / CAS / getAndAdd /
// getAndSet / lazySet / getAndBitwiseOr), so cell keys collide in the 64-entry
// table and failed attempts pile up past 65536; a WeakReference subclass and
// a SoftReference subclass with their own volatile fields updated hot while
// their referents stay reachable (get() must return them) and after the
// referents are dropped (get() may clear; printed as cleared-or-same);
// System.gc() between passes. Cold, hot, a late holder class, again.
//
// Run:
//   javac -d out R21Fz2UpdBudget1.java
//   java -Xmx256m -cp out R21Fz2UpdBudget1                               # HotSpot 25
//   CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS=1 CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL=1 \
//     CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD=1 cratonvm -Xmx256m -cp out R21Fz2UpdBudget1
//   the same with -XX:+UseG1GC and -XX:+UseZGC; without CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD (ref half unarmed)
//   cratonvm -Xmx256m -cp out R21Fz2UpdBudget1, cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.concurrent.atomic.AtomicLongFieldUpdater;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;

public class R21Fz2UpdBudget1 {
    static final int WARM = 50_000;

    static class H {
        volatile int i;
        volatile long l;
        volatile Object o;
    }

    // 12 distinct holder classes (distinct class ids, same slots).
    static class H0 extends H { } static class H1 extends H { } static class H2 extends H { }
    static class H3 extends H { } static class H4 extends H { } static class H5 extends H { }
    static class H6 extends H { } static class H7 extends H { } static class H8 extends H { }
    static class H9 extends H { } static class H10 extends H { } static class H11 extends H { }

    // Classes declaring their OWN fields at varying slots.
    static class G0 { volatile int a; volatile long b; volatile Object c; }
    static class G1 { int pad0; volatile int a; volatile long b; volatile Object c; }
    static class G2 { long pad0; Object pad1; volatile int a; volatile long b; volatile Object c; }
    static class G3 extends G2 { volatile int a3; volatile Object c3; }

    static final AtomicIntegerFieldUpdater<H> HI = AtomicIntegerFieldUpdater.newUpdater(H.class, "i");
    static final AtomicLongFieldUpdater<H> HL = AtomicLongFieldUpdater.newUpdater(H.class, "l");
    static final AtomicReferenceFieldUpdater<H, Object> HO =
            AtomicReferenceFieldUpdater.newUpdater(H.class, Object.class, "o");

    static final AtomicIntegerFieldUpdater<G0> G0A = AtomicIntegerFieldUpdater.newUpdater(G0.class, "a");
    static final AtomicLongFieldUpdater<G1> G1B = AtomicLongFieldUpdater.newUpdater(G1.class, "b");
    static final AtomicReferenceFieldUpdater<G2, Object> G2C =
            AtomicReferenceFieldUpdater.newUpdater(G2.class, Object.class, "c");
    static final AtomicReferenceFieldUpdater<G3, Object> G3C =
            AtomicReferenceFieldUpdater.newUpdater(G3.class, Object.class, "c3");
    static final AtomicIntegerFieldUpdater<G3> G3A = AtomicIntegerFieldUpdater.newUpdater(G3.class, "a3");

    static final class MyWeak extends WeakReference<Object> {
        volatile Object extra;
        volatile int hits;

        MyWeak(Object r, ReferenceQueue<Object> q) {
            super(r, q);
        }
    }

    static final class MySoft extends SoftReference<Object> {
        volatile long stamp;
        volatile Object extra;

        MySoft(Object r) {
            super(r);
        }
    }

    static final AtomicReferenceFieldUpdater<MyWeak, Object> WX =
            AtomicReferenceFieldUpdater.newUpdater(MyWeak.class, Object.class, "extra");
    static final AtomicIntegerFieldUpdater<MyWeak> WH = AtomicIntegerFieldUpdater.newUpdater(MyWeak.class, "hits");
    static final AtomicLongFieldUpdater<MySoft> SS = AtomicLongFieldUpdater.newUpdater(MySoft.class, "stamp");
    static final VarHandle SX;
    static final VarHandle VI;
    static final VarHandle VO;

    static {
        try {
            SX = MethodHandles.lookup().findVarHandle(MySoft.class, "extra", Object.class);
            VI = MethodHandles.lookup().findVarHandle(H.class, "i", int.class);
            VO = MethodHandles.lookup().findVarHandle(H.class, "o", Object.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static H make(int k) {
        switch (k % 12) {
            case 0: return new H0(); case 1: return new H1(); case 2: return new H2(); case 3: return new H3();
            case 4: return new H4(); case 5: return new H5(); case 6: return new H6(); case 7: return new H7();
            case 8: return new H8(); case 9: return new H9(); case 10: return new H10(); default: return new H11();
        }
    }

    static long opsH(H h, int v) {
        long r = 0;
        HI.set(h, v);
        r += HI.get(h);
        r += HI.compareAndSet(h, v, v + 1) ? 1 : 0;
        r += HI.getAndAdd(h, 3);
        HI.lazySet(h, HI.get(h) * 2);
        r += HI.get(h);
        HL.set(h, (long) v << 33);
        r += HL.getAndIncrement(h) >>> 30;
        r += HL.compareAndSet(h, ((long) v << 33) + 1, 5) ? HL.get(h) : -HL.get(h);
        Object a = "a" + (v & 3);
        HO.set(h, a);
        r += HO.get(h) == a ? 10 : 0;
        r += HO.compareAndSet(h, a, h) ? 20 : 0;
        r += HO.getAndSet(h, null) == h ? 40 : 0;
        VI.setVolatile(h, v ^ 0x55);
        r += (int) VI.getAndBitwiseOr(h, 0x100);
        VO.setRelease(h, a);
        r += VO.getAcquire(h) == a ? 80 : 0;
        return r;
    }

    static long opsG(G0 g0, G1 g1, G2 g2, G3 g3, int v) {
        long r = 0;
        G0A.set(g0, v);
        r += G0A.incrementAndGet(g0);
        G1B.set(g1, -v);
        r += G1B.addAndGet(g1, 10);
        Object o = Integer.valueOf(v & 127);
        G2C.set(g2, o);
        r += G2C.get(g2) == o ? 1 : 0;
        G3C.set(g3, o);
        r += G3C.compareAndSet(g3, o, g3) ? 2 : 0;
        r += G3C.get(g3) == g3 ? 4 : 0;
        G2C.set(g3, "inherited");
        r += G2C.get(g3) == "inherited" ? 8 : 0;
        r += G3A.getAndIncrement(g3);
        return r + g3.c.hashCode() % 7;
    }

    static long opsRef(MyWeak w, MySoft s, Object referent, int v) {
        long r = 0;
        WX.set(w, referent);
        r += WX.get(w) == referent ? 1 : 0;
        r += WH.incrementAndGet(w) > 0 ? 2 : 0;
        r += w.get() == referent ? 4 : 0;
        SS.set(s, v);
        r += SS.getAndAdd(s, 1) == v ? 8 : 0;
        SX.setVolatile(s, w);
        r += SX.getVolatile(s) == w ? 16 : 0;
        r += s.get() == referent ? 32 : 0;
        return r;
    }

    public static void main(String[] args) {
        G0 g0 = new G0();
        G1 g1 = new G1();
        G2 g2 = new G2();
        G3 g3 = new G3();
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        Object referent = new int[] {42};
        MyWeak w = new MyWeak(referent, q);
        MySoft s = new MySoft(referent);
        StringBuilder sb = new StringBuilder("row cold");
        for (int k = 0; k < 12; k++) {
            sb.append(' ').append(opsH(make(k), k));
        }
        System.out.println(sb + " g=" + opsG(g0, g1, g2, g3, 5) + " ref=" + opsRef(w, s, referent, 5));
        long h = 0;
        H[] hs = new H[48];
        for (int k = 0; k < hs.length; k++) {
            hs[k] = make(k);
        }
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + opsH(hs[r % hs.length], r) + opsG(g0, g1, g2, g3, r) + opsRef(w, s, referent, r);
            if (r == WARM / 2) {
                System.gc();
            }
        }
        System.out.println("warm hash " + h + " hits=" + w.hits + " stamp=" + s.stamp);
        sb = new StringBuilder("row hot");
        for (int k = 0; k < 12; k++) {
            sb.append(' ').append(opsH(make(k), k));
        }
        System.out.println(sb + " g=" + opsG(new G0(), new G1(), new G2(), new G3(), 5) + " ref="
                + opsRef(w, s, referent, 5));
        // Drop the weak referent's other strong roots except `extra`.
        MyWeak w2 = new MyWeak(new int[] {7}, q);
        WX.set(w2, null);
        System.gc();
        Object got = w2.get();
        System.out.println("weak cleared-or-same " + (got == null || ((int[]) got)[0] == 7) + " extra=" + WX.get(w2)
                + " w.get==referent " + (w.get() == referent) + " w.extra==referent " + (WX.get(w) == referent));
        // Deopt trigger: a holder class first seen now.
        H late = new H() {
        };
        System.out.println("row late " + opsH(late, 9) + " " + opsH(make(3), 9) + " g3=" + opsG(g0, g1, g2, new G3() {
        }, 9));
        sb = new StringBuilder("row after");
        for (int k = 0; k < 12; k++) {
            sb.append(' ').append(opsH(make(k), k + 100));
        }
        System.out.println(sb + " ref=" + opsRef(new MyWeak(referent, q), new MySoft(referent), referent, 77));
        System.out.println("referent " + ((int[]) referent)[0]);
    }
}
