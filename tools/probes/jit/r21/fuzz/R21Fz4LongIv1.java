// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 15, lane fuzz4: counted loops with a LONG induction
// variable, int indices derived from it, and mixed int/long accumulators.
//
// Risk targeted: the IR long-loop recognition and the lcmp+If fusion
// (`CRATONVM_JIT_IR_LCMP_COMPARE_FUSE`, the "lcmp compare fusion" fix of
// af21db8b22), loop opts that narrow a long induction variable to 32 bits,
// range checks on `a[(int) l]` and `a[(int) (l - base)]` where `l` crosses
// 2^31 and 2^32 (the narrowing must happen per trip, not once), the unsigned
// long compare (`Long.compareUnsigned` as a loop condition), strides of
// 2^32 + 1 and negative strides that wrap through Long.MIN_VALUE, and the
// int-versus-long accumulator split (`int s += a[i]` wraps, `long t += a[i]`
// does not; `i * i` in int versus `(long) i * i`). Each kernel returns a
// hash of its accumulators and its trip count; out-of-range cases return the
// trip count at the AIOOBE and the faulting index text in the cold pass.
//
// Passes: `cold`, `warm` (each kernel 60k calls on its safe argument set, plus
// one long-running call per kernel so OSR compiles too), `edge` (the edge
// arguments again after compilation), `late` (a never-loaded `Step` subclass
// changes the stride source of the virtual-stride kernel: deopt), `rewarm`,
// `final`.
//
// Run:
//   javac -d out R21Fz4LongIv1.java
//   java -Xmx256m -cp out R21Fz4LongIv1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz4LongIv1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz4LongIv1
//   cratonvm --nojit -Xmx256m -cp out R21Fz4LongIv1
//   CRATONVM_JIT_IR_LCMP_COMPARE_FUSE=0 cratonvm ...      # control arm
//   CRATONVM_JIT_IR_LICM_HOIST_COUNTED=1 cratonvm ...
// HotSpot expectation: every line identical on every arm.

public class R21Fz4LongIv1 {
    static final int WARM = 60_000;

    static String lastMsg;

    static int[] filled(int n) {
        int[] a = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = (i + 1) * 0x01010101;
        }
        return a;
    }

    static long mix(long h, long v) {
        return (h ^ v) * 0x100000001B3L;
    }

    // l crosses 2^31; index is l - base.
    static long cross31(int[] a, long from, long to, long base) {
        long h = 1, sl = 0;
        int t = 0;
        try {
            for (long l = from; l < to; l++) {
                h = mix(h, a[(int) (l - base)]);
                sl += l;
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            lastMsg = e.getMessage();
            return mix(mix(h, sl), -1000 - t);
        }
        return mix(mix(h, sl), t);
    }

    // Index is (int) l itself: crosses 2^32, so (int) l goes -16 .. -1 then 0 ..
    static long narrow(int[] a, long from, long to) {
        long h = 3;
        int t = 0;
        try {
            for (long l = from; l < to; l++) {
                h = mix(h, a[(int) l]);
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            lastMsg = e.getMessage();
            return mix(h, -1000 - t);
        }
        return mix(h, t);
    }

    // Masked narrowing never faults.
    static long narrowMask(int[] a, long from, long to) {
        long h = 5;
        int t = 0;
        for (long l = from; l < to; l++) {
            h = mix(h, a[(int) l & 31]);
            h = mix(h, (int) l);
            h = mix(h, (short) l);
            t++;
        }
        return mix(h, t);
    }

    // Negative stride through Long.MIN_VALUE, `!=` condition.
    static long wrapNe(long from, long to, long st) {
        long h = 7;
        int t = 0;
        for (long l = from; l != to; l += st) {
            h = mix(h, l);
            if (++t > 1000) {
                break;
            }
        }
        return mix(h, t);
    }

    // Unsigned compare as the loop condition; bounded by a trip cap.
    static long unsignedCond(long lim, long st) {
        long h = 11;
        int t = 0;
        for (long l = 0; Long.compareUnsigned(l, lim) < 0; l += st) {
            h = mix(h, l);
            if (++t >= 40) {
                break;
            }
        }
        return mix(h, t);
    }

    // Stride 2^32 + 1: (int) (l & 15) is t & 15.
    static long bigStride(int[] a, long lim) {
        long h = 13;
        int t = 0;
        for (long l = 0; l < lim; l += 0x100000001L) {
            h = mix(h, a[(int) (l & 15)]);
            h = mix(h, (int) l);
            t++;
        }
        return mix(h, t);
    }

    // Down-count long loop indexing directly.
    static long down(int[] a, long n) {
        long h = 17;
        int t = 0;
        try {
            for (long l = n - 1; l >= 0; l--) {
                h = mix(h, a[(int) l]);
                t++;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            lastMsg = e.getMessage();
            return mix(h, -1000 - t);
        }
        return mix(h, t);
    }

    // int induction, int vs long accumulators.
    static long accum(int[] a, int n) {
        int si = 0;
        long sl = 0;
        int sq = 0;
        long lsq = 0;
        for (int i = 0; i < n; i++) {
            int v = a[i % a.length];
            si += v;
            sl += v;
            sq += i * i;
            lsq += (long) i * i;
        }
        long h = mix(19, si);
        h = mix(h, sl);
        h = mix(h, sq);
        return mix(h, lsq);
    }

    // Outer long, inner int bounded by (int) outer.
    static long nested(int[] a, long from, long to) {
        long h = 23;
        int t = 0;
        for (long o = from; o < to; o++) {
            int lim = (int) o & 15;
            for (int i = 0; i < lim; i++) {
                h = mix(h, a[i] + o);
                t++;
            }
        }
        return mix(h, t);
    }

    // Long induction near Long.MAX_VALUE with `<=`: the last trip is MAX, the
    // next increment wraps and the loop ends only because of the trip cap.
    static long inclMax(long from) {
        long h = 29;
        int t = 0;
        for (long l = from; l <= Long.MAX_VALUE; l++) {
            h = mix(h, l);
            if (++t >= 12) {
                break;
            }
        }
        return mix(h, t);
    }

    abstract static class Step {
        abstract long step(long l);
    }

    static final class One extends Step {
        long step(long l) {
            return 1L;
        }
    }

    // Never touched before the `late` pass.
    static final class Jump extends Step {
        long step(long l) {
            return (l & 1) == 0 ? 0x80000000L : 3L;
        }
    }

    static long virtStride(int[] a, Step st, long from, long to) {
        long h = 31;
        int t = 0;
        for (long l = from; l < to; l += st.step(l)) {
            h = mix(h, a[(int) (l & 63)]);
            h = mix(h, (int) (l >>> 31));
            t++;
        }
        return mix(h, t);
    }

    static void row(String pass, String name, long v, boolean msg) {
        System.out.println(pass + " " + name + " " + Long.toHexString(v) + (msg && lastMsg != null ? " msg=" + lastMsg : ""));
        lastMsg = null;
    }

    static final long P31 = 0x80000000L;
    static final long P32 = 0x100000000L;

    static void edges(String pass, boolean msg) {
        int[] a32 = filled(32);
        int[] a64 = filled(64);
        row(pass, "cross31.ok", cross31(a32, P31 - 16, P31 + 16, P31 - 16), msg);
        row(pass, "cross31.over", cross31(a32, P31 - 16, P31 + 20, P31 - 16), msg);
        row(pass, "cross31.neg", cross31(a32, P31 - 16, P31 + 16, P31 - 15), msg);
        row(pass, "cross31.p32", cross31(a32, P32 - 8, P32 + 8, P32 - 8), msg);
        row(pass, "narrow.p32", narrow(a32, P32, P32 + 20), msg);
        row(pass, "narrow.belowP32", narrow(a32, P32 - 16, P32 + 4), msg);
        row(pass, "narrow.p31", narrow(a32, P31 - 2, P31 + 2), msg);
        row(pass, "narrowMask.p32", narrowMask(a32, P32 - 40, P32 + 40), msg);
        row(pass, "narrowMask.p31", narrowMask(a32, P31 - 40, P31 + 40), msg);
        row(pass, "narrowMask.min", narrowMask(a32, Long.MIN_VALUE, Long.MIN_VALUE + 40), msg);
        row(pass, "wrapNe.min", wrapNe(Long.MIN_VALUE + 3, Long.MAX_VALUE - 2, -1L), msg);
        row(pass, "wrapNe.max", wrapNe(Long.MAX_VALUE - 3, Long.MIN_VALUE + 2, 1L), msg);
        row(pass, "wrapNe.step3", wrapNe(Long.MAX_VALUE - 4, Long.MIN_VALUE + 4, 3L), msg);
        row(pass, "unsignedCond.top", unsignedCond(0xF800000000000000L, 0x3000000000000000L), msg);
        row(pass, "unsignedCond.mid", unsignedCond(0x8000000000000001L, 0x2000000000000000L), msg);
        row(pass, "unsignedCond.zero", unsignedCond(0L, 1L), msg);
        row(pass, "bigStride.40", bigStride(a32, 40 * 0x100000001L), msg);
        row(pass, "bigStride.p32", bigStride(a32, P32 + 2), msg);
        row(pass, "down.ok", down(a32, 32), msg);
        row(pass, "down.over", down(a32, 33), msg);
        row(pass, "down.p32", down(a32, P32 + 1), msg);
        row(pass, "accum.big", accum(new int[] {Integer.MAX_VALUE, Integer.MAX_VALUE - 1, Integer.MIN_VALUE + 7}, 70000), msg);
        row(pass, "nested.p32", nested(a32, P32 - 20, P32 + 20), msg);
        row(pass, "inclMax", inclMax(Long.MAX_VALUE - 5), msg);
        row(pass, "virt.one", virtStride(a64, new One(), P31 - 50, P31 + 50), msg);
    }

    static long warm() {
        int[] a32 = filled(32);
        int[] a64 = filled(64);
        Step one = new One();
        long h = 0;
        for (int k = 0; k < WARM; k++) {
            long base = 1000 + (k & 7);
            h += cross31(a32, base, base + 30, base);
            h += narrow(a32, k & 7, 30);
            h += narrowMask(a32, base, base + 20);
            h += wrapNe(base + 10, base, -1L);
            h += unsignedCond(base & 31, 1L);
            h += bigStride(a32, 20 * 0x100000001L);
            h += down(a32, 30);
            h += accum(a32, 40);
            h += nested(a32, base, base + 3);
            h += inclMax(Long.MAX_VALUE - 20);
            h += virtStride(a64, one, base, base + 30);
        }
        // Long single calls: OSR.
        h += narrowMask(a32, 0, 3_000_000);
        h += accum(a32, 3_000_000);
        h += virtStride(a64, one, P32 - 1_000_000, P32 + 1_000_000);
        h += bigStride(a32, 2_000_000 * 0x100000001L);
        return h;
    }

    public static void main(String[] args) {
        edges("cold", true);
        System.out.println("warm h=" + Long.toHexString(warm()));
        edges("edge", false);
        int[] a64 = filled(64);
        Step jump = new Jump();
        row("late", "virt.jump", virtStride(a64, jump, P31 - 50, P31 + 0x200000000L), false);
        row("late", "virt.jumpP32", virtStride(a64, jump, P32 - 7, P32 + 0x300000000L), false);
        row("late", "virt.one", virtStride(a64, new One(), P31 - 50, P31 + 50), false);
        System.out.println("rewarm h=" + Long.toHexString(warm()));
        edges("final", false);
        row("final", "virt.jump", virtStride(a64, jump, P31 - 50, P31 + 0x200000000L), false);
        System.out.println("done");
    }
}
