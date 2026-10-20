// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: UPDATERS21-1, the reference volatile field
// arm (`CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD`: `getReferenceVolatile` /
// `Acquire` / `Opaque` as one MOV, `putReferenceVolatile` as XCHG behind the
// reference-store gate, `putReferenceRelease` / `Opaque` as MOV), plus the
// `--compatible` updater value check (`CRATONVM_ARFU_VALUE_CHECK`, default ON).
//
// Risk targeted: a reference store outside the getfield/putfield lowering
// that misses the GC's card mark / SATB pre-barrier loses a reference across
// a collection (a young object stored into an old holder, read back after a
// GC, must still be the same object with the same contents), and a reference
// load that misses a barrier reads a stale pointer. Rows:
//   * an OLD holder (allocated first, survives many GCs) gets fresh young
//     values through ARFU.set / lazySet / compareAndSet / getAndSet, then a
//     System.gc() and allocation churn, then the values are read back and
//     their payloads checked;
//   * a VarHandle setVolatile / setRelease / setOpaque twin of the same
//     shape (the `Acquire` / `Opaque` / `Release` bodies);
//   * null values, CAS against null, CAS failure;
//   * a value of the wrong type through a raw updater: ClassCastException
//     (the JDK's valueCheck), in both modes;
//   * a subclass holder (another class id at the same offset).
// Every value is identified by its payload, never by identity hash.
//
// Run:
//   javac -d out R21FzUpdRef1.java
//   java -Xmx256m -cp out R21FzUpdRef1                                    # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzUpdRef1
//   CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS=1 CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL=1 CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD=1 cratonvm ...
//   the same arm with -XX:+UseG1GC and with -XX:+UseZGC (the arm must decline under ZGC)
//   cratonvm --compatible ...; CRATONVM_ARFU_VALUE_CHECK=0 cratonvm --compatible ... (kill control)
//   cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;

public class R21FzUpdRef1 {
    static final int WARM = 40_000;

    static final class Val {
        final int id;
        final int[] payload;

        Val(int id) {
            this.id = id;
            this.payload = new int[] {id, id * 3, id ^ 0x5A5A};
        }

        boolean ok() {
            return payload[0] == id && payload[1] == id * 3 && payload[2] == (id ^ 0x5A5A);
        }

        @Override
        public String toString() {
            return "V" + id + (ok() ? "" : "!corrupt");
        }
    }

    static class Holder {
        volatile Val v;
        volatile Object o;
    }

    static final class SubHolder extends Holder {
        volatile Val w;
    }

    static final AtomicReferenceFieldUpdater<Holder, Val> V =
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, Val.class, "v");
    static final AtomicReferenceFieldUpdater<Holder, Object> O =
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, Object.class, "o");
    static final VarHandle VH;

    static {
        try {
            VH = MethodHandles.lookup().findVarHandle(Holder.class, "v", Val.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static int churn(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            int[] junk = new int[16 + (i & 31)];
            junk[0] = i;
            s += junk[0] & 1;
        }
        return s;
    }

    static int step(Holder h, int r) {
        int c = 0;
        V.set(h, new Val(r));
        c += V.get(h).id;
        V.lazySet(h, new Val(r + 1));
        c += V.get(h).id;
        Val cur = V.get(h);
        c += V.compareAndSet(h, cur, new Val(r + 2)) ? 1 : 0;
        c += V.compareAndSet(h, cur, new Val(-1)) ? 1 : 0;
        Val prev = V.getAndSet(h, new Val(r + 3));
        c += prev.id;
        O.set(h, (r & 1) == 0 ? new Val(r + 4) : "str" + r);
        Object o = O.get(h);
        c += o instanceof Val vv ? vv.id : o.toString().length();
        VH.setRelease(h, new Val(r + 5));
        c += ((Val) VH.getAcquire(h)).id;
        VH.setOpaque(h, new Val(r + 6));
        c += ((Val) VH.getOpaque(h)).id;
        VH.setVolatile(h, new Val(r + 7));
        c += ((Val) VH.getVolatile(h)).id;
        return c;
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static String wrongType(Holder h) {
        AtomicReferenceFieldUpdater raw = V;
        StringBuilder sb = new StringBuilder();
        try {
            raw.set(h, "not a Val");
            sb.append("set-ok:").append(h.v == null ? "null" : h.v.getClass().getSimpleName());
        } catch (ClassCastException e) {
            sb.append("set-CCE:").append(e.getMessage());
        }
        try {
            raw.compareAndSet(h, h.v, Integer.valueOf(3));
            sb.append(" cas-ok");
        } catch (ClassCastException e) {
            sb.append(" cas-CCE:").append(e.getMessage());
        }
        try {
            raw.getAndSet(h, new Object());
            sb.append(" gas-ok");
        } catch (ClassCastException e) {
            sb.append(" gas-CCE:").append(e.getMessage());
        }
        try {
            raw.set("not a holder", new Val(1));
            sb.append(" holder-ok");
        } catch (ClassCastException e) {
            sb.append(" holder-CCE:").append(e.getMessage());
        }
        return sb.toString();
    }

    static String nulls(Holder h) {
        V.set(h, null);
        boolean a = V.compareAndSet(h, null, new Val(9));
        boolean b = V.compareAndSet(h, null, new Val(10));
        Val g = V.getAndSet(h, null);
        return a + "," + b + "," + g + "," + V.get(h);
    }

    static final Holder OLD = new Holder();
    static final SubHolder OLD_SUB = new SubHolder();

    static void rows(String tag) {
        int c1 = step(OLD, 100);
        int c2 = step(OLD_SUB, 200);
        churn(200_000);
        System.gc();
        churn(50_000);
        System.out.println("row " + tag + " step " + c1 + " " + c2 + " after-gc " + OLD.v + " " + OLD_SUB.v + " "
                + OLD.o + " " + OLD_SUB.o);
        System.out.println("row " + tag + " nulls " + nulls(new Holder()) + " " + nulls(OLD_SUB));
        Holder fresh = new Holder();
        System.out.println("row " + tag + " wrong " + wrongType(fresh));
        V.set(OLD, new Val(77));
    }

    public static void main(String[] args) {
        rows("cold");
        long h = 0;
        Holder[] hs = new Holder[64];
        for (int i = 0; i < hs.length; i++) {
            hs[i] = (i & 1) == 0 ? new Holder() : new SubHolder();
        }
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + step(hs[r & 63], r);
            if ((r & 8191) == 0) {
                churn(100_000);
            }
        }
        System.gc();
        int bad = 0;
        long ids = 0;
        for (Holder x : hs) {
            if (!x.v.ok()) {
                bad++;
            }
            ids += x.v.id;
        }
        System.out.println("warm hash " + h + " bad=" + bad + " ids=" + ids + " old=" + OLD.v);
        rows("hot");
        rows("after");
    }
}
