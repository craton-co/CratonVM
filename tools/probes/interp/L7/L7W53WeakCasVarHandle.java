// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 53, lane L7: the interpreter's signature-
// polymorphic site cache serves the four WEAK `VarHandle` CAS access modes
// through its instance-field CAS arm (`sig_poly_site::is_boolean_cas_access_mode`,
// under `CRATONVM_VH_INTERP_CAS_FAST`), as it served `compareAndSet` since
// JIT round 16. Each mode runs cold (r0) and warm (r1..r3) at one site per
// mode and kind, succeeding and failing, then the refusals the arm must
// leave to the native at the same sites: a null receiver (NPE), a receiver of
// another class (ClassCastException), and a read-only handle on a final
// field (UnsupportedOperationException). Only exception class names are
// printed (the message texts are lane L4's).
//
// Positive control (`--nojit`): `CRATONVM_DBG_FIELD_SITE=1`, FINAL line,
// `vh-cas-fast: N` with N above 800000 (the loop's weak CAS calls), and
// near 0 on the base (only strong `compareAndSet` sites were served) or with
// `CRATONVM_VH_INTERP_CAS_FAST=0`.
//
// A weak CAS may fail spuriously on HotSpot; on a field no other thread
// touches it does not in practice, and the probe retries each success in a
// bounded loop so the line is deterministic either way.
//
// Expected (HotSpot 25.0.3, `java L7W53WeakCasVarHandle`, and `-Xint`):
//   r0 int true false 5 true false 7 true false 9 true false 11
//   r0 long true false 5 true false 7 true false 9 true false 11
//   r0 ref true false b true false c true false d true false e
//   r0 refuse java.lang.NullPointerException java.lang.ClassCastException java.lang.UnsupportedOperationException
//   r1 int true false 5 true false 7 true false 9 true false 11
//   r1 long true false 5 true false 7 true false 9 true false 11
//   r1 ref true false b true false c true false d true false e
//   r1 refuse java.lang.NullPointerException java.lang.ClassCastException java.lang.UnsupportedOperationException
//   r2 int true false 5 true false 7 true false 9 true false 11
//   r2 long true false 5 true false 7 true false 9 true false 11
//   r2 ref true false b true false c true false d true false e
//   r2 refuse java.lang.NullPointerException java.lang.ClassCastException java.lang.UnsupportedOperationException
//   r3 int true false 5 true false 7 true false 9 true false 11
//   r3 long true false 5 true false 7 true false 9 true false 11
//   r3 ref true false b true false c true false d true false e
//   r3 refuse java.lang.NullPointerException java.lang.ClassCastException java.lang.UnsupportedOperationException
//   loop 400000 400000
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L7W53WeakCasVarHandle {
    static final class H {
        int i;
        long j;
        String s;
        final int fin = 1;
    }

    static final class Other {
        int i;
    }

    static final VarHandle I;
    static final VarHandle J;
    static final VarHandle S;
    static final VarHandle FIN;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            I = l.findVarHandle(H.class, "i", int.class);
            J = l.findVarHandle(H.class, "j", long.class);
            S = l.findVarHandle(H.class, "s", String.class);
            FIN = l.findVarHandle(H.class, "fin", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    // One site per mode and kind; `Object h` so a foreign receiver reaches it.
    static boolean wI(Object h, int a, int b) { return I.weakCompareAndSet(h, a, b); }
    static boolean wIPlain(Object h, int a, int b) { return I.weakCompareAndSetPlain(h, a, b); }
    static boolean wIAcq(Object h, int a, int b) { return I.weakCompareAndSetAcquire(h, a, b); }
    static boolean wIRel(Object h, int a, int b) { return I.weakCompareAndSetRelease(h, a, b); }

    static boolean wJ(H h, long a, long b) { return J.weakCompareAndSet(h, a, b); }
    static boolean wJPlain(H h, long a, long b) { return J.weakCompareAndSetPlain(h, a, b); }
    static boolean wJAcq(H h, long a, long b) { return J.weakCompareAndSetAcquire(h, a, b); }
    static boolean wJRel(H h, long a, long b) { return J.weakCompareAndSetRelease(h, a, b); }

    static boolean wS(H h, String a, String b) { return S.weakCompareAndSet(h, a, b); }
    static boolean wSPlain(H h, String a, String b) { return S.weakCompareAndSetPlain(h, a, b); }
    static boolean wSAcq(H h, String a, String b) { return S.weakCompareAndSetAcquire(h, a, b); }
    static boolean wSRel(H h, String a, String b) { return S.weakCompareAndSetRelease(h, a, b); }

    static boolean wFin(H h, int a, int b) { return FIN.weakCompareAndSet(h, a, b); }

    interface IntCas { boolean cas(H h, int a, int b); }
    interface LongCas { boolean cas(H h, long a, long b); }
    interface RefCas { boolean cas(H h, String a, String b); }

    // A weak CAS may fail spuriously: retry a CAS that should succeed.
    static boolean ok(IntCas c, H h, int a, int b) {
        for (int k = 0; k < 1000; k++) {
            if (c.cas(h, a, b)) {
                return true;
            }
        }
        return false;
    }

    static boolean okJ(LongCas c, H h, long a, long b) {
        for (int k = 0; k < 1000; k++) {
            if (c.cas(h, a, b)) {
                return true;
            }
        }
        return false;
    }

    static boolean okS(RefCas c, H h, String a, String b) {
        for (int k = 0; k < 1000; k++) {
            if (c.cas(h, a, b)) {
                return true;
            }
        }
        return false;
    }

    static String name(Throwable t) {
        return t.getClass().getName();
    }

    static void round(int r) {
        H h = new H();
        StringBuilder sb = new StringBuilder("r" + r + " int");
        h.i = 4;
        IntCas[] ints = {
            (x, a, b) -> wI(x, a, b),
            (x, a, b) -> wIPlain(x, a, b),
            (x, a, b) -> wIAcq(x, a, b),
            (x, a, b) -> wIRel(x, a, b),
        };
        for (IntCas c : ints) {
            int before = h.i;
            sb.append(' ').append(ok(c, h, before, before + 1));
            sb.append(' ').append(c.cas(h, before, before + 100));
            sb.append(' ').append(h.i);
            h.i = h.i + 1;
        }
        System.out.println(sb);

        sb = new StringBuilder("r" + r + " long");
        h.j = 4L;
        LongCas[] longs = {
            (x, a, b) -> wJ(x, a, b),
            (x, a, b) -> wJPlain(x, a, b),
            (x, a, b) -> wJAcq(x, a, b),
            (x, a, b) -> wJRel(x, a, b),
        };
        for (LongCas c : longs) {
            long before = h.j;
            sb.append(' ').append(okJ(c, h, before, before + 1));
            sb.append(' ').append(c.cas(h, before, before + 100));
            sb.append(' ').append(h.j);
            h.j = h.j + 1;
        }
        System.out.println(sb);

        sb = new StringBuilder("r" + r + " ref");
        String[] seq = {"a", "b", "c", "d", "e"};
        h.s = seq[0];
        RefCas[] refs = {
            (x, a, b) -> wS(x, a, b),
            (x, a, b) -> wSPlain(x, a, b),
            (x, a, b) -> wSAcq(x, a, b),
            (x, a, b) -> wSRel(x, a, b),
        };
        for (int k = 0; k < refs.length; k++) {
            sb.append(' ').append(okS(refs[k], h, seq[k], seq[k + 1]));
            sb.append(' ').append(refs[k].cas(h, seq[k], "zz"));
            sb.append(' ').append(h.s);
        }
        System.out.println(sb);

        String npe;
        try {
            wI(null, 0, 1);
            npe = "none";
        } catch (Throwable t) {
            npe = name(t);
        }
        String cce;
        try {
            wI(new Other(), 0, 1);
            cce = "none";
        } catch (Throwable t) {
            cce = name(t);
        }
        String uoe;
        try {
            wFin(h, 1, 2);
            uoe = "none";
        } catch (Throwable t) {
            uoe = name(t);
        }
        System.out.println("r" + r + " refuse " + npe + " " + cce + " " + uoe);
    }

    public static void main(String[] args) {
        for (int r = 0; r < 4; r++) {
            round(r);
        }
        H h = new H();
        long n = 0;
        long m = 0;
        h.i = 0;
        for (int k = 0; k < 200_000; k++) {
            while (!wI(h, k * 2, k * 2 + 1)) { }
            n++;
            while (!wIPlain(h, k * 2 + 1, k * 2 + 2)) { }
            n++;
        }
        h.s = "p";
        for (int k = 0; k < 200_000; k++) {
            while (!wS(h, h.s, (k & 1) == 0 ? "q" : "p")) { }
            m++;
            while (!wSRel(h, h.s, (k & 1) == 0 ? "p" : "q")) { }
            m++;
        }
        System.out.println("loop " + n + " " + (m / 2 * 2 == m ? m : -1));
    }
}
