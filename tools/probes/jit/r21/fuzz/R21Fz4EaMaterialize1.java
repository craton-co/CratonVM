// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 15, lane fuzz4: scalar-replaced objects that are
// materialized late -- at an uncommon trap, inside a lock, mid-loop, or
// carried by an exception -- and identity facts about them.
//
// Risk targeted: escape analysis and scalar replacement in the IR tier
// (`jit/src/escape_analysis.rs`, `jit/src/ea_ir_bridge.rs`;
// `CRATONVM_JIT_EA_LOOP_CARRIED_FIELDS`, `CRATONVM_JIT_EA_REF_COMPARE_FOLD`,
// `CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM`, `CRATONVM_JIT_EA_CROSS_BLOCK_LOADS`,
// `CRATONVM_JIT_EA_FENCE_LOCAL_USE`, `CRATONVM_JIT_EA_FRESH_TYPECHECK_FOLD`,
// `CRATONVM_JIT_IR_EA_NARROW_PHI`) and the deopt rematerialization:
//   * `trap`:   a point mutated in a loop escapes only through an Observer whose
//     sole loaded implementation is a no-op; a never-loaded `Keep` observer
//     arrives late, so the compiled body deopts with the object virtual and
//     must rebuild it with its CURRENT field values;
//   * `lockEsc`: a non-escaping object locked (lock elision), then escaping
//     inside the synchronized region on a rare iteration: Thread.holdsLock must
//     be true for the materialized object, recursively locked too, and the lock
//     must be free after the region (another thread acquires it);
//   * `carried`: an accumulator object updated in a loop escapes mid-loop and
//     the later updates must be visible through the escaped reference;
//   * `identity`: `a == b` for two fresh objects, `p == p` through a phi, a
//     materialized object's identityHashCode stable across escape, Integer /
//     Long.valueOf cache identity (-128..127 equal, 128 not) under boxing
//     elimination;
//   * `exArr`:  a fresh array escaping as an exception's payload;
//   * `unusedArr`: an unused `new int[n]` with n = -1 must still throw
//     NegativeArraySizeException, and an unused `new long[n]` warmed with
//     small n and then called with n = 1 << 27 (1 GiB) must still throw
//     OutOfMemoryError under -Xmx256m (an unused array may be
//     removed only when its allocation provably succeeds);
//   * `typeFold`: instanceof / getClass on a fresh object through an Object
//     phi, including a late subclass.
//
// Passes: `cold`, `warm` (60k+ calls with the no-op observer), `late`
// (`Keep` observer and `Sub` points loaded), `rewarm`, `final`.
// Every printed value is a count, a field value or a boolean.
//
// Run:
//   javac -d out R21Fz4EaMaterialize1.java
//   java -Xmx256m -cp out R21Fz4EaMaterialize1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz4EaMaterialize1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz4EaMaterialize1
//   cratonvm --nojit -Xmx256m -cp out R21Fz4EaMaterialize1
//   CRATONVM_JIT_EA_REF_COMPARE_FOLD=1 CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM=1 cratonvm ...
//   CRATONVM_JIT_EA_LOOP_CARRIED_FIELDS=0 cratonvm ...     # control arm
//   CRATONVM_JIT_IR_HONOUR_DISABLE_SCALAR_REPLACEMENT=1 cratonvm ... (with -XX:-EliminateAllocations if accepted)
// HotSpot expectation: every line identical on every arm.

public class R21Fz4EaMaterialize1 {
    static final int WARM = 60_000;

    static class Pt {
        int x;
        int y;
        long z;

        Pt(int x, int y) {
            this.x = x;
            this.y = y;
        }
    }

    // Never instantiated before the `late` pass.
    static final class Sub extends Pt {
        int w = 77;

        Sub(int x, int y) {
            super(x, y);
        }
    }

    static final class Acc {
        long sum;
        int n;
    }

    interface Obs {
        void see(Object o, int i);
    }

    static final class Nop implements Obs {
        public void see(Object o, int i) {
        }
    }

    static Object[] kept = new Object[64];
    static int keptN;

    // Never touched before the `late` pass.
    static final class Keep implements Obs {
        public void see(Object o, int i) {
            if (keptN < kept.length) {
                kept[keptN++] = o;
            }
        }
    }

    static long describeKept() {
        long h = 0;
        for (int i = 0; i < keptN; i++) {
            Object o = kept[i];
            if (o instanceof Pt p) {
                h = h * 31 + p.x * 7 + p.y * 3 + p.z + (o instanceof Sub s ? s.w : 0);
            } else if (o instanceof Acc a) {
                h = h * 31 + a.sum + a.n;
            } else if (o instanceof int[] arr) {
                h = h * 31 + arr.length + arr[0] + arr[arr.length - 1];
            } else {
                h = h * 31 + 999;
            }
        }
        return h;
    }

    static void resetKept() {
        for (int i = 0; i < kept.length; i++) {
            kept[i] = null;
        }
        keptN = 0;
    }

    // ---- trap -----------------------------------------------------------------

    static long trap(Obs obs, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            Pt p = new Pt(i, -i);
            for (int j = 0; j < 4; j++) {
                p.x += j;
                p.z += (long) p.x * j;
            }
            if ((i & 7) == 3) {
                obs.see(p, i);
                p.y += 1000;
            }
            s += p.x + p.y + p.z;
        }
        return s;
    }

    // ---- lockEsc --------------------------------------------------------------

    static int holdsCount;
    static int holdsMiss;

    static long lockEsc(Obs obs, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            Pt p = new Pt(i, i);
            synchronized (p) {
                p.x++;
                synchronized (p) {
                    p.y += 2;
                    if (i % 13 == 5) {
                        obs.see(p, i);
                        if (Thread.holdsLock(p)) {
                            holdsCount++;
                        } else {
                            holdsMiss++;
                        }
                    }
                }
                if (i % 13 == 5) {
                    if (Thread.holdsLock(p)) {
                        holdsCount++;
                    } else {
                        holdsMiss++;
                    }
                }
            }
            if (i % 13 == 5 && Thread.holdsLock(p)) {
                holdsMiss += 100;
            }
            s += p.x + p.y;
        }
        return s;
    }

    static String lockFree() throws InterruptedException {
        int free = 0, busy = 0;
        for (int i = 0; i < keptN; i++) {
            Object o = kept[i];
            final boolean[] got = new boolean[1];
            Thread t = new Thread(() -> {
                synchronized (o) {
                    got[0] = true;
                }
            });
            t.start();
            t.join(5000);
            if (got[0]) {
                free++;
            } else {
                busy++;
            }
        }
        return "free=" + free + " busy=" + busy;
    }

    // ---- carried --------------------------------------------------------------

    static long carried(Obs obs, int[] a, int n) {
        Acc acc = new Acc();
        for (int i = 0; i < n; i++) {
            acc.sum += a[i % a.length];
            acc.n++;
            if (i == n / 3) {
                obs.see(acc, i);
            }
        }
        return acc.sum * 1000 + acc.n;
    }

    // ---- identity -------------------------------------------------------------

    static int idBits(Obs obs, int i, boolean c) {
        Pt a = new Pt(1, 2);
        Pt b = new Pt(1, 2);
        Pt p = c ? a : b;
        int bits = 0;
        if (a == b) bits |= 1;
        if (p == a) bits |= 2;
        if (p == b) bits |= 4;
        if (p != p) bits |= 8;
        int h1 = System.identityHashCode(a);
        if ((i & 63) == 17) {
            obs.see(a, i);
        }
        if (h1 == System.identityHashCode(a)) bits |= 16;
        Integer x = i & 255;
        Integer y = i & 255;
        if (x == y) bits |= 32;
        Long lx = (long) (i & 255) - 128;
        Long ly = (long) (i & 255) - 128;
        if (lx == ly) bits |= 64;
        Integer big = 1000 + (i & 3);
        Integer big2 = 1000 + (i & 3);
        if (big == big2) bits |= 128;
        if (Integer.valueOf(127) == Integer.valueOf(127)) bits |= 256;
        if (Integer.valueOf(128) == Integer.valueOf(128)) bits |= 512;
        return bits;
    }

    static long identity(Obs obs, int n) {
        long h = 0;
        int[] hist = new int[1024];
        for (int i = 0; i < n; i++) {
            hist[idBits(obs, i, (i & 1) == 0)]++;
        }
        for (int k = 0; k < hist.length; k++) {
            if (hist[k] != 0) {
                h = h * 1_000_003L + (k * 100_000L + hist[k]);
            }
        }
        return h;
    }

    // ---- exArr ----------------------------------------------------------------

    static final class Carry extends RuntimeException {
        final int[] payload;

        Carry(int[] p) {
            super("carry", null, false, false);
            payload = p;
        }
    }

    static int exArrStep(int i) {
        int[] tmp = new int[4];
        tmp[0] = i;
        tmp[3] = i * 2;
        tmp[1] = tmp[0] + tmp[3];
        if (i % 97 == 50) {
            throw new Carry(tmp);
        }
        return tmp[1] + tmp[2];
    }

    static long exArr(int n) {
        long s = 0;
        long caught = 0;
        for (int i = 0; i < n; i++) {
            try {
                s += exArrStep(i);
            } catch (Carry c) {
                caught = caught * 31 + c.payload[0] + c.payload[1] + c.payload[2] + c.payload[3] + c.payload.length;
            }
        }
        return s * 7 + caught;
    }

    // ---- unusedArr ------------------------------------------------------------

    static int unusedArr(int n) {
        int[] unused = new int[n];
        return n + 1;
    }

    static int unusedLong(int n) {
        long[] unused = new long[n];
        return n + 2;
    }

    static String unusedRow(int n) {
        try {
            return "ok " + unusedArr(n);
        } catch (NegativeArraySizeException e) {
            return "NASE " + e.getMessage();
        }
    }

    static String hugeRow() {
        try {
            return "ok " + unusedLong(1 << 27);
        } catch (OutOfMemoryError e) {
            return "OOME";
        }
    }

    // ---- typeFold -------------------------------------------------------------

    static int typeFold(int i, boolean late) {
        Object o;
        if (late && (i & 3) == 1) {
            o = new Sub(i, i);
        } else if ((i & 1) == 0) {
            o = new Pt(i, 0);
        } else {
            o = new Acc();
        }
        int bits = 0;
        if (o instanceof Pt) bits |= 1;
        if (o instanceof Sub) bits |= 2;
        if (o instanceof Acc) bits |= 4;
        if (o.getClass() == Pt.class) bits |= 8;
        if (o.getClass() == Sub.class) bits |= 16;
        if (o instanceof Comparable) bits |= 32;
        return bits;
    }

    static long typeFoldLoop(int n, boolean late) {
        long[] hist = new long[64];
        for (int i = 0; i < n; i++) {
            hist[typeFold(i, late)]++;
        }
        long h = 0;
        for (int k = 0; k < 64; k++) {
            h = h * 31 + hist[k] * (k + 1);
        }
        return h;
    }

    // ---- driver ---------------------------------------------------------------

    static void facts(String pass, Obs obs, boolean late) throws InterruptedException {
        int[] a = {3, 1, 4, 1, 5, 9, 2, 6};
        resetKept();
        System.out.println(pass + " trap " + trap(obs, 200) + " kept=" + keptN + " desc=" + describeKept());
        resetKept();
        holdsCount = 0;
        holdsMiss = 0;
        System.out.println(pass + " lockEsc " + lockEsc(obs, 120) + " holds=" + holdsCount + " miss=" + holdsMiss
            + " kept=" + keptN + " desc=" + describeKept() + " " + lockFree());
        resetKept();
        System.out.println(pass + " carried " + carried(obs, a, 300) + " kept=" + keptN + " desc=" + describeKept());
        resetKept();
        System.out.println(pass + " identity " + identity(obs, 2048) + " kept=" + keptN + " desc=" + describeKept());
        resetKept();
        System.out.println(pass + " exArr " + exArr(500));
        System.out.println(pass + " unused.3 " + unusedRow(3) + " unused.0 " + unusedRow(0) + " unused.m1 " + unusedRow(-1)
            + " unused.min " + unusedRow(Integer.MIN_VALUE));
        System.out.println(pass + " huge " + hugeRow());
        System.out.println(pass + " typeFold " + typeFoldLoop(400, late));
    }

    static long warm(Obs nop) {
        int[] a = {3, 1, 4, 1, 5, 9, 2, 6};
        long h = 0;
        for (int k = 0; k < WARM; k++) {
            h += trap(nop, 16);
            h += lockEsc(nop, 14);
            h += carried(nop, a, 24);
            h += idBits(nop, k, (k & 1) == 0);
            h += exArr(3);
            h += unusedArr(k & 15);
            h += unusedLong(k & 31);
            h += typeFold(k, false);
        }
        h += exArr(200_000);
        h += trap(nop, 300_000);
        h += identity(nop, 200_000);
        holdsCount = 0;
        holdsMiss = 0;
        return h;
    }

    public static void main(String[] args) throws InterruptedException {
        Obs nop = new Nop();
        facts("cold", nop, false);
        System.out.println("warm h=" + Long.toHexString(warm(nop)));
        facts("hot", nop, false);
        Obs keep = new Keep();
        facts("late", keep, true);
        facts("late2", nop, true);
        System.out.println("rewarm h=" + Long.toHexString(warm(nop)));
        facts("final", keep, true);
        System.out.println("done");
    }
}
