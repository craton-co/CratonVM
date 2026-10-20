// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 53, lane L7: two positive receiver slots per
// interpreter cast site (`CRATONVM_CAST_MEMO_TWO_POSITIVE`,
// `CastSite::older_positive_receiver`; proposal
// `i52-L7-proposal-two-positive-receiver-slots-per-cast-site-20261016`). Each
// shape runs cold (r0) and warm (r1..r3), so the slots one round fills answer
// the next; every site that memoised two receivers is then asked about a
// receiver it must NOT admit:
//
//   cls     `checkcast Base` / `instanceof Base` alternating SubA, SubB, then
//           a third subclass SubC (evicts the oldest slot), then Other at the
//           same sites (false, and a ClassCastException)
//   iface   the same against an interface (Sq, Ci, then Tri, then Other)
//   lam     `(Runnable)` and `instanceof Runnable` alternating two Runnable
//           lambdas, then a Supplier lambda at the same sites (false, CCE)
//   catch   one `catch (RuntimeException)` row taking IllegalStateException
//           and IllegalArgumentException in turn, then an Error that must
//           pass it to the outer row
//   loop    200000 executions of the bimorphic sites (both slots hit)
//
// Positive control: `CRATONVM_DBG_FIELD_SITE=1`, FINAL line,
// `cast-two-positive: fill=N older_hit=M` with M in the hundreds of
// thousands with the switch on and `fill=0 older_hit=0` with
// `CRATONVM_CAST_MEMO_TWO_POSITIVE=0`. `CRATONVM_DBG_CAST_MEMO_CROSSCHECK=1`
// must print no `[CAST-MEMO-CROSSCHECK]` line.
//
// Expected (HotSpot 25.0.3, `java L7W53TwoPositiveCastMemo`, and `-Xint`):
//   r0 cls 1 2 true true 3 true false java.lang.ClassCastException
//   r0 iface 1 2 true true 3 true false java.lang.ClassCastException
//   r0 lam 1 2 true true false java.lang.ClassCastException
//   r0 catch ise iae ise error
//   r1 cls 1 2 true true 3 true false java.lang.ClassCastException
//   r1 iface 1 2 true true 3 true false java.lang.ClassCastException
//   r1 lam 1 2 true true false java.lang.ClassCastException
//   r1 catch ise iae ise error
//   r2 cls 1 2 true true 3 true false java.lang.ClassCastException
//   r2 iface 1 2 true true 3 true false java.lang.ClassCastException
//   r2 lam 1 2 true true false java.lang.ClassCastException
//   r2 catch ise iae ise error
//   r3 cls 1 2 true true 3 true false java.lang.ClassCastException
//   r3 iface 1 2 true true 3 true false java.lang.ClassCastException
//   r3 lam 1 2 true true false java.lang.ClassCastException
//   r3 catch ise iae ise error
//   loop 200000 200000 200000
import java.util.function.Supplier;

public class L7W53TwoPositiveCastMemo {
    static class Base { int v() { return 0; } }
    static final class SubA extends Base { int v() { return 1; } }
    static final class SubB extends Base { int v() { return 2; } }
    static final class SubC extends Base { int v() { return 3; } }
    static final class Other {}

    interface Shape { int size(); }
    static final class Sq implements Shape { public int size() { return 1; } }
    static final class Ci implements Shape { public int size() { return 2; } }
    static final class Tri implements Shape { public int size() { return 3; } }

    // One site each: every call below reaches the same bytecode.
    static int castBase(Object o) {
        return ((Base) o).v();
    }

    static boolean isBase(Object o) {
        return o instanceof Base;
    }

    static int castShape(Object o) {
        return ((Shape) o).size();
    }

    static boolean isShape(Object o) {
        return o instanceof Shape;
    }

    static Runnable castRunnable(Object o) {
        return (Runnable) o;
    }

    static boolean isRunnable(Object o) {
        return o instanceof Runnable;
    }

    static String catchRow(RuntimeException toThrow, Error err) {
        try {
            try {
                if (err != null) {
                    throw err;
                }
                throw toThrow;
            } catch (RuntimeException e) {
                return e instanceof IllegalStateException ? "ise" : "iae";
            }
        } catch (Error e) {
            return "error";
        }
    }

    static String cce(Runnable r) {
        try {
            r.run();
            return "none";
        } catch (ClassCastException e) {
            return e.getClass().getName();
        }
    }

    static void round(int r) {
        Object a = new SubA();
        Object b = new SubB();
        Object c = new SubC();
        Object other = new Other();
        int va = castBase(a);
        int vb = castBase(b);
        boolean ia = isBase(a);
        boolean ib = isBase(b);
        int vc = castBase(c);
        boolean ic = isBase(c);
        boolean io = isBase(other);
        String cls = cce(() -> castBase(other));
        System.out.println("r" + r + " cls " + va + " " + vb + " " + ia + " " + ib + " " + vc + " "
                + ic + " " + io + " " + cls);

        Object sq = new Sq();
        Object ci = new Ci();
        Object tri = new Tri();
        int sa = castShape(sq);
        int sb = castShape(ci);
        boolean sia = isShape(sq);
        boolean sib = isShape(ci);
        int sc = castShape(tri);
        boolean sic = isShape(tri);
        boolean sio = isShape(other);
        String iface = cce(() -> castShape(other));
        System.out.println("r" + r + " iface " + sa + " " + sb + " " + sia + " " + sib + " " + sc
                + " " + sic + " " + sio + " " + iface);

        int[] hits = new int[1];
        Runnable l1 = () -> hits[0] += 1;
        Runnable l2 = () -> hits[0] += 10;
        Supplier<String> sup = () -> "s";
        hits[0] = 0;
        castRunnable(l1).run();
        int h1 = hits[0];
        hits[0] = 0;
        castRunnable(l2).run();
        castRunnable(l1).run();
        castRunnable(l2).run();
        int h2 = hits[0] == 21 ? 2 : -1;
        boolean la = isRunnable(l1);
        boolean lb = isRunnable(l2);
        boolean ls = isRunnable(sup);
        String lam = cce(() -> castRunnable(sup));
        System.out.println("r" + r + " lam " + h1 + " " + h2 + " " + la + " " + lb + " " + ls + " "
                + lam);

        String c1 = catchRow(new IllegalStateException(), null);
        String c2 = catchRow(new IllegalArgumentException(), null);
        String c3 = catchRow(new IllegalStateException(), null);
        String c4 = catchRow(null, new AssertionError());
        System.out.println("r" + r + " catch " + c1 + " " + c2 + " " + c3 + " " + c4);
    }

    public static void main(String[] args) {
        for (int r = 0; r < 4; r++) {
            round(r);
        }
        Object a = new SubA();
        Object b = new SubB();
        Object sq = new Sq();
        Object ci = new Ci();
        int[] hits = new int[1];
        Runnable l1 = () -> hits[0]++;
        Runnable l2 = () -> hits[0]++;
        long cls = 0;
        long iface = 0;
        long lam = 0;
        for (int i = 0; i < 100_000; i++) {
            cls += isBase(a) ? 1 : 0;
            cls += isBase(b) ? 1 : 0;
            iface += castShape(sq) > 0 ? 1 : 0;
            iface += castShape(ci) > 0 ? 1 : 0;
            castRunnable(l1).run();
            castRunnable(l2).run();
        }
        lam = hits[0];
        System.out.println("loop " + cls + " " + iface + " " + lam);
    }
}
