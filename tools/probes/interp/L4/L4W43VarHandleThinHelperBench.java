// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 43, lane L4: the JIT-mode baseline that proposal
// `i37-L4-proposal-thin-varhandle-helpers-know-their-call-site` asks to
// measure before and after its change. Every row is a hot loop over a
// `static final` field `VarHandle` whose site the single-pass tier binds to a
// thin helper (`jit/src/lib.rs` `varhandle_{read,write,cas}_helper_slot`:
// one reference coordinate, the value kinds the slots cover).
//
// Rows (ns/op on stderr):
//
//   get-int        `(int) I.get(h)`            read slot, int
//   getvol-long    `(long) L.getVolatile(h)`   read slot, long
//   get-ref        `(Object) R.get(h)`         read slot, REF_OBJECT
//   set-int        `I.set(h, i)`               write slot, int
//   setrel-long    `L.setRelease(h, i)`        write slot, long
//   set-ref        `R.set(h, o)`               write slot, reference
//   cas-int        `I.compareAndSet(h, a, b)`  CAS slot, int
//   cas-ref        `R.compareAndSet(h, a, b)`  CAS slot, reference
//   get-int-exact  `(int) IX.get(h)` on an invoke-exact handle, the site's
//                  own type: the exactness gate is open for the whole run
//                  (a control for the proposal's per-site judge)
//
// How to run (JIT mode, the default; each row warms first):
//
//   cratonvm --java-home <jdk25> -cp <dir> L4W43VarHandleThinHelperBench
//
// Engagement: `CRATONVM_DBG=jit-method-stats` reports the helpers'
// `VarHandle.read=`, `VarHandle.write=`, `VarHandle.cas=` served/declined
// counts; a zero served count means the rows measured the generic funnel.
// Interleave against the base build, fat LTO, minima per core.
//
// Stdout is a deterministic checksum per row and must equal HotSpot 25's
// (`-Xint` too). HotSpot 25 (25.0.3) prints:
//   get-int 12499997500000
//   getvol-long 12499997500000
//   get-ref 5000000
//   set-int 12499997500000
//   setrel-long 12499997500000
//   set-ref 2500000
//   cas-int 10000000
//   cas-ref 5000000
//   get-int-exact 12499997500000
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L4W43VarHandleThinHelperBench {
    static final int WARMUP = 200_000;
    static final int ITERS = 5_000_000;

    static final class H {
        int i;
        long l;
        Object r;
    }

    static final VarHandle I;
    static final VarHandle L;
    static final VarHandle R;
    static final VarHandle IX;

    static {
        try {
            MethodHandles.Lookup lk = MethodHandles.lookup();
            I = lk.findVarHandle(H.class, "i", int.class);
            L = lk.findVarHandle(H.class, "l", long.class);
            R = lk.findVarHandle(H.class, "r", Object.class);
            IX = I.withInvokeExactBehavior();
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    interface Row {
        long run(H h, int iters);
    }

    static long getInt(H h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            h.i = k;
            s += (int) I.get(h);
        }
        return s;
    }

    static long getVolLong(H h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            h.l = k;
            s += (long) L.getVolatile(h);
        }
        return s;
    }

    static long getRef(H h, int n) {
        long s = 0;
        Object a = "a";
        for (int k = 0; k < n; k++) {
            h.r = a;
            s += ((Object) R.get(h)) == a ? 1 : 0;
        }
        return s;
    }

    static long setInt(H h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            I.set(h, k);
            s += h.i;
        }
        return s;
    }

    static long setRelLong(H h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            L.setRelease(h, (long) k);
            s += h.l;
        }
        return s;
    }

    static long setRef(H h, int n) {
        long s = 0;
        Object a = "a";
        Object b = "b";
        for (int k = 0; k < n; k++) {
            R.set(h, (k & 1) == 0 ? a : b);
            s += h.r == a ? 1 : 0;
        }
        return s;
    }

    static long casInt(H h, int n) {
        long s = 0;
        h.i = 0;
        for (int k = 0; k < n; k++) {
            s += I.compareAndSet(h, k, k + 1) ? 1 : 0;
        }
        return s + h.i;
    }

    static long casRef(H h, int n) {
        long s = 0;
        Object a = "a";
        Object b = "b";
        h.r = a;
        for (int k = 0; k < n; k++) {
            if ((k & 1) == 0) {
                s += R.compareAndSet(h, a, b) ? 1 : 0;
            } else {
                s += R.compareAndSet(h, b, a) ? 1 : 0;
            }
        }
        return s;
    }

    static long getIntExact(H h, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            h.i = k;
            s += (int) IX.get(h);
        }
        return s;
    }

    static void row(String name, Row r) {
        r.run(new H(), WARMUP);
        H h = new H();
        long t0 = System.nanoTime();
        long sum = r.run(h, ITERS);
        long ns = System.nanoTime() - t0;
        System.out.println(name + " " + sum);
        System.err.printf("%-14s %8.2f ns/op%n", name, (double) ns / ITERS);
    }

    public static void main(String[] args) {
        row("get-int", L4W43VarHandleThinHelperBench::getInt);
        row("getvol-long", L4W43VarHandleThinHelperBench::getVolLong);
        row("get-ref", L4W43VarHandleThinHelperBench::getRef);
        row("set-int", L4W43VarHandleThinHelperBench::setInt);
        row("setrel-long", L4W43VarHandleThinHelperBench::setRelLong);
        row("set-ref", L4W43VarHandleThinHelperBench::setRef);
        row("cas-int", L4W43VarHandleThinHelperBench::casInt);
        row("cas-ref", L4W43VarHandleThinHelperBench::casRef);
        row("get-int-exact", L4W43VarHandleThinHelperBench::getIntExact);
    }
}
