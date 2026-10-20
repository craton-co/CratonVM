// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 15, lane fuzz4: loop-invariant code motion and loop
// unswitching of loads and TRAPPING operations.
//
// Risk targeted: LICM (`CRATONVM_JIT_IR_LICM_MEM_EDGE`, `..._HOIST_COUNTED`,
// `..._ARRAY_LOAD`, `..._DIRECT_STATIC`, `..._STATIC_READ_NOT_BARRIER`,
// `..._BEFORE_UNROLL`, `..._GUARD_ATTRIBUTION`, `CRATONVM_JIT_ARITH_LICM_TRIM_ENTERED`)
// must not
//   * hoist a trapping invariant (`100 / d`, `o.f` with `o == null`,
//     `a[k]` with k out of range, `arr.length` of null, a checkcast that
//     fails) out of a loop that runs ZERO times, or ahead of an earlier
//     iteration's side effect (the exception must come at the right trip, with
//     the earlier stores done);
//   * hoist a field / static / array-element load across a call that writes it
//     (directly, through an alias, or through an interface whose only loaded
//     implementation is a no-op until a late class arrives: the deopt
//     trigger), across a volatile read, or across `synchronized`;
//   * unswitch on a field that the loop body itself toggles.
// The exception rows print the exception class, the trip at which it came and
// the side effects done before it; messages only in the cold pass.
//
// Passes: `cold`, `warm` (60k calls of each kernel with arguments that never
// trap and hooks that do nothing), `edge`, `late` (`Writer`, a never-loaded
// Hook, now writes the loaded fields), `rewarm`, `final`.
//
// Run:
//   javac -d out R21Fz4LicmTrap1.java
//   java -Xmx256m -cp out R21Fz4LicmTrap1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz4LicmTrap1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz4LicmTrap1
//   cratonvm --nojit -Xmx256m -cp out R21Fz4LicmTrap1
//   CRATONVM_JIT_IR_LICM_HOIST_COUNTED=1 CRATONVM_JIT_IR_LICM_DIRECT_STATIC=1 cratonvm ...
//   CRATONVM_JIT_IR_LICM_BEFORE_UNROLL=1 CRATONVM_JIT_IR_LICM_ARRAY_LOAD=1 cratonvm ...
// HotSpot expectation: every line identical on every arm.

public class R21Fz4LicmTrap1 {
    static final int WARM = 60_000;

    static int S;
    static int stores;
    static String lastMsg;

    static final class Box {
        int f;
        int[] arr = {1, 2, 3, 4};
        volatile int v;
        boolean flag;
        Object o = "str";
    }

    interface Hook {
        void on(Box b, int i);
    }

    static final class Nop implements Hook {
        public void on(Box b, int i) {
        }
    }

    // Never touched before the `late` pass.
    static final class Writer implements Hook {
        public void on(Box b, int i) {
            if (i == 3) {
                b.f += 1000;
                b.arr[0] += 7;
                S += 50;
                b.arr = new int[] {9, 9, 9, 9};
            }
        }
    }

    // ---- trapping invariants -------------------------------------------------

    static String res(int t, long s) {
        return "trips=" + t + " sum=" + s + " stores=" + stores;
    }

    static String exc(String pass, RuntimeException e, int t, long s) {
        lastMsg = e.getMessage();
        return "EXC " + e.getClass().getName() + " " + res(t, s);
    }

    static String divInv(int n, int d) {
        int t = 0;
        long s = 0;
        stores = 0;
        try {
            for (int i = 0; i < n; i++) {
                stores++;
                s += 100 / d;
                t++;
            }
        } catch (ArithmeticException e) {
            return exc("", e, t, s);
        }
        return res(t, s);
    }

    static String nullField(int n, Box b) {
        int t = 0;
        long s = 0;
        stores = 0;
        try {
            for (int i = 0; i < n; i++) {
                stores++;
                s += b.f;
                t++;
            }
        } catch (NullPointerException e) {
            return exc("", e, t, s);
        }
        return res(t, s);
    }

    static String nullLen(int n, int[] a) {
        int t = 0;
        long s = 0;
        stores = 0;
        try {
            for (int i = 0; i < n; i++) {
                stores++;
                s += a.length;
                t++;
            }
        } catch (NullPointerException e) {
            return exc("", e, t, s);
        }
        return res(t, s);
    }

    static String badIndex(int n, int[] a, int k) {
        int t = 0;
        long s = 0;
        stores = 0;
        try {
            for (int i = 0; i < n; i++) {
                stores++;
                s += a[k];
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            return exc("", e, t, s);
        }
        return res(t, s);
    }

    static String badCast(int n, Object o) {
        int t = 0;
        long s = 0;
        stores = 0;
        try {
            for (int i = 0; i < n; i++) {
                stores++;
                s += ((String) o).length();
                t++;
            }
        } catch (ClassCastException e) {
            return exc("", e, t, s);
        }
        return res(t, s);
    }

    // The trap comes after a guarded first trip: i == 0 takes the safe path.
    static String divLater(int n, int d) {
        int t = 0;
        long s = 0;
        stores = 0;
        try {
            for (int i = 0; i < n; i++) {
                stores++;
                s += (i == 0) ? 1 : 100 / d;
                t++;
            }
        } catch (ArithmeticException e) {
            return exc("", e, t, s);
        }
        return res(t, s);
    }

    // Unswitching: the trapping arm is only taken when mode == 1.
    static String unswitchDiv(int n, int mode, int d) {
        int t = 0;
        long s = 0;
        stores = 0;
        try {
            for (int i = 0; i < n; i++) {
                stores++;
                if (mode == 1) {
                    s += i / d;
                } else {
                    s += i;
                }
                t++;
            }
        } catch (ArithmeticException e) {
            return exc("", e, t, s);
        }
        return res(t, s);
    }

    // ---- loads across writes -------------------------------------------------

    static void bump(Box b, int i) {
        if (i == 5) {
            b.f += 100;
        }
    }

    static long fieldAcrossCall(Box b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += b.f;
            bump(b, i);
        }
        return s;
    }

    static long fieldAlias(Box b, Box c, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += b.f;
            c.f = i;
        }
        return s;
    }

    static long staticAcrossHook(Box b, Hook h, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s = s * 31 + S + b.f + b.arr[0] + b.arr.length;
            h.on(b, i);
        }
        return s;
    }

    static long acrossVolatile(Box b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += b.f;
            b.v = i;
            if (b.v == 4) {
                b.f = -b.f;
            }
        }
        return s;
    }

    static final Object LOCK = new Object();

    static long acrossSync(Box b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += S + b.arr[1];
            synchronized (LOCK) {
                if (i == 2) {
                    S++;
                    b.arr[1] = 40;
                }
            }
        }
        return s;
    }

    static long toggled(Box b, int[] a, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            if (b.flag) {
                s += a[i];
            } else {
                s -= a[i];
            }
            if (i == n / 2) {
                b.flag = !b.flag;
            }
        }
        return s;
    }

    static long stringReassigned(String str, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s = s * 7 + str.length();
            if (i == 2) {
                str = str + str;
            }
        }
        return s;
    }

    static Box fresh(int f) {
        Box b = new Box();
        b.f = f;
        return b;
    }

    static void edges(String pass, boolean msg) {
        String[] rows = {
            "divInv.n0d0 " + divInv(0, 0),
            "divInv.n3d0 " + divInv(3, 0),
            "divLater.n3d0 " + divLater(3, 0),
            "divLater.n1d0 " + divLater(1, 0),
            "nullField.n0 " + nullField(0, null),
            "nullField.n2 " + nullField(2, null),
            "nullLen.n0 " + nullLen(0, null),
            "nullLen.n2 " + nullLen(2, null),
            "badIndex.n0 " + badIndex(0, new int[4], 4),
            "badIndex.n2 " + badIndex(2, new int[4], -1),
            "badCast.n0 " + badCast(0, Integer.valueOf(3)),
            "badCast.n2 " + badCast(2, Integer.valueOf(3)),
            "unswitch.m0d0 " + unswitchDiv(5, 0, 0),
            "unswitch.m1d0 " + unswitchDiv(5, 1, 0),
            "unswitch.m1dm1 " + unswitchDiv(5, 1, -1),
        };
        for (String r : rows) {
            System.out.println(pass + " " + r);
        }
        if (msg) {
            System.out.println(pass + " lastMsg " + lastMsg);
            String m;
            try {
                badIndex(1, new int[4], 9);
            } finally {
                m = lastMsg;
            }
            System.out.println(pass + " msg.badIndex " + m);
            divInv(1, 0);
            System.out.println(pass + " msg.div " + lastMsg);
        }
        S = 0;
        Box b = fresh(1);
        System.out.println(pass + " fieldAcrossCall " + fieldAcrossCall(b, 10) + " f=" + b.f);
        b = fresh(1);
        System.out.println(pass + " fieldAlias.same " + fieldAlias(b, b, 10) + " f=" + b.f);
        Box c = fresh(2);
        b = fresh(1);
        System.out.println(pass + " fieldAlias.other " + fieldAlias(b, c, 10) + " f=" + b.f + "," + c.f);
        b = fresh(3);
        System.out.println(pass + " acrossVolatile " + acrossVolatile(b, 10) + " f=" + b.f + " v=" + b.v);
        S = 0;
        b = fresh(0);
        System.out.println(pass + " acrossSync " + acrossSync(b, 6) + " S=" + S + " arr1=" + b.arr[1]);
        b = fresh(0);
        int[] a = {1, 2, 3, 4, 5, 6, 7, 8};
        System.out.println(pass + " toggled.f " + toggled(b, a, 8) + " flag=" + b.flag);
        System.out.println(pass + " toggled.t " + toggled(b, a, 8) + " flag=" + b.flag);
        System.out.println(pass + " stringReassigned " + stringReassigned("ab", 6));
        S = 0;
        b = fresh(1);
        System.out.println(pass + " hook.nop " + staticAcrossHook(b, new Nop(), 8) + " f=" + b.f + " S=" + S);
    }

    static long warm() {
        long h = 0;
        Hook nop = new Nop();
        int[] a = new int[64];
        for (int i = 0; i < a.length; i++) {
            a[i] = i * 3 + 1;
        }
        Box b = fresh(1);
        Box c = fresh(2);
        for (int k = 0; k < WARM; k++) {
            int n = 20 + (k & 7);
            h += divInv(n, 3 + (k & 1)).length();
            h += divLater(n, 7).length();
            h += nullField(n, b).length();
            h += nullLen(n, a).length();
            h += badIndex(n, a, k & 63).length();
            h += badCast(n, "abc").length();
            h += unswitchDiv(n, k & 1, 5).length();
            b.f = k & 3;
            h += fieldAcrossCall(b, n);
            h += fieldAlias(b, c, n);
            h += acrossVolatile(b, n);
            S = 0;
            h += acrossSync(b, n);
            b.arr[1] = 2;
            h += toggled(b, a, n);
            h += stringReassigned("xy", 6);
            S = 0;
            h += staticAcrossHook(b, nop, n);
        }
        return h;
    }

    static void late(String pass) {
        Hook w = new Writer();
        S = 0;
        Box b = fresh(1);
        System.out.println(pass + " hook.writer " + staticAcrossHook(b, w, 8) + " f=" + b.f + " S=" + S + " arr=" + b.arr[0]);
        S = 0;
        b = fresh(1);
        System.out.println(pass + " hook.nop " + staticAcrossHook(b, new Nop(), 8) + " f=" + b.f + " S=" + S);
        S = 0;
        b = fresh(1);
        System.out.println(pass + " hook.writer2 " + staticAcrossHook(b, w, 8) + " f=" + b.f + " S=" + S + " arr=" + b.arr[0]);
    }

    public static void main(String[] args) {
        edges("cold", true);
        System.out.println("warm h=" + Long.toHexString(warm()));
        edges("edge", false);
        late("late");
        System.out.println("rewarm h=" + Long.toHexString(warm()));
        edges("final", false);
        late("final");
        System.out.println("done");
    }
}
