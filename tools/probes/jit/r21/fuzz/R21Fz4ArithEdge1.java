// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 15, lane fuzz4: integer arithmetic corner cases, cold and
// compiled in both tiers.
//
// Risk targeted: the IR constant-division lowering (`CRATONVM_JIT_IR_CONST_DIV`,
// magic-number multiply for /7, /-7, /3, /641, /MAX_VALUE, power-of-two
// divisors with negative dividends, /-1 and /MIN_VALUE), the cold division
// fix-ups (`CRATONVM_JIT_IR_DIV_COLD_FIXUPS`: MIN_VALUE / -1 must give
// MIN_VALUE and % -1 must give 0, never a #DE trap), the multiply strength
// reduction (`CRATONVM_JIT_IR_MUL_STRENGTH_REDUCE`: *3, *5, *9, *-6, *65537,
// *MAX_VALUE, *MIN_VALUE), the scalar identities and scalar intrinsics
// (`CRATONVM_JIT_IR_SCALAR_IDENTITIES`, `CRATONVM_JIT_IR_SCALAR_INTRINSICS`,
// `CRATONVM_JIT_IR_SCALAR_RANGE_FOLD`: Math.floorDiv/floorMod/ceilDiv,
// multiplyHigh, unsignedMultiplyHigh, multiplyFull, the *Exact family and
// their ArithmeticException texts, Integer/Long.compress/expand, reverse,
// bitCount, rotate by negative distances, compareUnsigned, divideUnsigned),
// and the shift masking rules (int shift counts are & 31, long & 63: counts
// 32, 33, 63, 64, 65, -1, -31, MIN_VALUE, and constant counts >= 32 that javac
// keeps as written). Every operation is folded into a per-row FNV hash, so a
// single wrong bit changes the row.
//
// Passes: `cold` tables (one row per dividend, hashed over every divisor),
// explicit `edge` rows for the famous pairs, `hot` (200k iterations of a
// kernel that calls every op family through an abstract input source, with a
// divisor of 0 every 4096th iteration: implicit ArithmeticException inside
// compiled code, counted by class), `late` (a never-loaded Src subclass
// arrives: CHA-devirtualized kernels go not entrant), `rehot`, and the `after`
// tables, which must equal the `cold` tables.
//
// Run:
//   javac -d out R21Fz4ArithEdge1.java
//   java -Xmx256m -cp out R21Fz4ArithEdge1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz4ArithEdge1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz4ArithEdge1
//   cratonvm --nojit -Xmx256m -cp out R21Fz4ArithEdge1
//   CRATONVM_JIT_IR_MUL_STRENGTH_REDUCE=1 cratonvm -Xmx256m -cp out R21Fz4ArithEdge1
//   CRATONVM_JIT_IR_CONST_DIV=0 cratonvm ...          # control arm (generic idiv)
//   CRATONVM_JIT_IR_DIV_COLD_FIXUPS=0 cratonvm ...    # control arm
//   CRATONVM_JIT_FORCE_C2=1 cratonvm ...              # IR tier from the start (if supported)
// HotSpot expectation: every line identical on every arm.

public class R21Fz4ArithEdge1 {
    static final int HOT = 200_000;

    static final int[] IV = {0, 1, -1, 2, -2, 3, 7, -7, 8, -8, 31, 32, 33, 63, 64,
        Integer.MIN_VALUE, Integer.MIN_VALUE + 1, Integer.MAX_VALUE, Integer.MAX_VALUE - 1,
        0x55555555, 0xAAAAAAAA, 46341, -46341, 65536, 1000000007};

    static final long[] LV = {0L, 1L, -1L, 2L, -2L, 3L, 7L, -7L, 8L, 31L, 32L, 63L, 64L, 65L,
        Long.MIN_VALUE, Long.MIN_VALUE + 1, Long.MAX_VALUE, Long.MAX_VALUE - 1,
        Integer.MIN_VALUE, Integer.MAX_VALUE, 0xFFFFFFFFL, 0x100000000L, -0x100000000L,
        3037000500L, -3037000500L, 0x5555555555555555L, 0xAAAAAAAAAAAAAAAAL,
        1000000007L * 1000000009L};

    static int excCount;

    static long m(long h, long v) {
        return (h ^ v) * 0x100000001B3L;
    }

    static String hx(long v) {
        return Long.toHexString(v);
    }

    // ---- int families -----------------------------------------------------

    static long divI(int a, int b) {
        long h = 17;
        h = m(h, a / 7); h = m(h, a % 7);
        h = m(h, a / -7); h = m(h, a % -7);
        h = m(h, a / 8); h = m(h, a % 8);
        h = m(h, a / -8); h = m(h, a % -8);
        h = m(h, a / -1); h = m(h, a % -1);
        h = m(h, a / 1); h = m(h, a % 1);
        h = m(h, a / 3); h = m(h, a % 3);
        h = m(h, a / 641); h = m(h, a % 1000000007);
        h = m(h, a / Integer.MIN_VALUE); h = m(h, a % Integer.MIN_VALUE);
        h = m(h, a / Integer.MAX_VALUE); h = m(h, a % Integer.MAX_VALUE);
        h = m(h, a / 0x40000000); h = m(h, a % 0x40000000);
        h = m(h, (a / 16) * 16 - a);
        if (b != 0) {
            h = m(h, a / b); h = m(h, a % b);
        }
        int bb = b == 0 ? 1 : b;
        h = m(h, Math.floorDiv(a, bb)); h = m(h, Math.floorMod(a, bb));
        h = m(h, Math.floorDiv(a, -3)); h = m(h, Math.floorMod(a, -3));
        h = m(h, Math.floorMod(a, 8)); h = m(h, Math.floorMod(a, -8));
        h = m(h, Math.ceilDiv(a, bb)); h = m(h, Math.ceilMod(a, bb));
        h = m(h, Integer.divideUnsigned(a, bb)); h = m(h, Integer.remainderUnsigned(a, bb));
        h = m(h, Integer.divideUnsigned(a, 10)); h = m(h, Integer.remainderUnsigned(a, -2));
        return h;
    }

    static long shI(int a, int b) {
        long h = 19;
        h = m(h, a << b); h = m(h, a >> b); h = m(h, a >>> b);
        h = m(h, a << 31); h = m(h, a >> 31); h = m(h, a >>> 31);
        h = m(h, a << 32); h = m(h, a >> 33); h = m(h, a >>> 63);
        h = m(h, a << -1); h = m(h, a >>> -31); h = m(h, a >> Integer.MIN_VALUE);
        h = m(h, (a << b) >> b); h = m(h, (a << 24) >> 24); h = m(h, (a << 16) >>> 16);
        h = m(h, a * 3); h = m(h, a * -1); h = m(h, a * 5); h = m(h, a * 9);
        h = m(h, a * 65537); h = m(h, a * 0x7fffffff); h = m(h, a * Integer.MIN_VALUE);
        h = m(h, a * -6); h = m(h, a * 0); h = m(h, a * 1024);
        h = m(h, a * b); h = m(h, (long) a * b); h = m(h, Math.multiplyFull(a, b));
        h = m(h, (byte) (a * 3)); h = m(h, (short) (a + b)); h = m(h, (char) (a - b));
        h = m(h, -a); h = m(h, ~a); h = m(h, a & -a); h = m(h, a ^ (a >> 31));
        h = m(h, Integer.rotateLeft(a, b)); h = m(h, Integer.rotateRight(a, -b));
        h = m(h, Integer.reverse(a)); h = m(h, Integer.reverseBytes(a)); h = m(h, Integer.bitCount(a));
        h = m(h, Integer.numberOfLeadingZeros(a)); h = m(h, Integer.numberOfTrailingZeros(a));
        h = m(h, Integer.highestOneBit(a)); h = m(h, Integer.lowestOneBit(a)); h = m(h, Integer.signum(a));
        h = m(h, Integer.compress(a, b)); h = m(h, Integer.expand(a, b));
        h = m(h, Integer.compareUnsigned(a, b)); h = m(h, Integer.compare(a, b));
        h = m(h, Integer.toUnsignedLong(a));
        h = m(h, Math.abs(a)); h = m(h, Math.max(a, b)); h = m(h, Math.min(a, b));
        h = m(h, a < 0 ? -a : a);
        h = m(h, (a >>> 1) + (b >>> 1) + (a & b & 1));
        h = m(h, Integer.MAX_VALUE + a); h = m(h, a - Integer.MIN_VALUE);
        h = m(h, a == Integer.MIN_VALUE ? 1 : 0); h = m(h, (a - 1) < a ? 1 : 0);
        h = m(h, (a + 1) > a ? 1 : 0);
        return h;
    }

    static final int OVF = 0x0BAD0BAD;

    static int ovf(ArithmeticException e) {
        String msg = e.getMessage();
        return OVF ^ (msg == null ? 0 : msg.hashCode());
    }

    static int addX(int a, int b) { try { return Math.addExact(a, b); } catch (ArithmeticException e) { return ovf(e); } }
    static int subX(int a, int b) { try { return Math.subtractExact(a, b); } catch (ArithmeticException e) { return ovf(e); } }
    static int mulX(int a, int b) { try { return Math.multiplyExact(a, b); } catch (ArithmeticException e) { return ovf(e); } }
    static int negX(int a) { try { return Math.negateExact(a); } catch (ArithmeticException e) { return ovf(e); } }
    static int absX(int a) { try { return Math.absExact(a); } catch (ArithmeticException e) { return ovf(e); } }
    static int incX(int a) { try { return Math.incrementExact(a); } catch (ArithmeticException e) { return ovf(e); } }
    static int decX(int a) { try { return Math.decrementExact(a); } catch (ArithmeticException e) { return ovf(e); } }
    static int divX(int a, int b) { try { return Math.divideExact(a, b); } catch (ArithmeticException e) { return ovf(e); } }
    static int fdivX(int a, int b) { try { return Math.floorDivExact(a, b); } catch (ArithmeticException e) { return ovf(e); } }
    static int cdivX(int a, int b) { try { return Math.ceilDivExact(a, b); } catch (ArithmeticException e) { return ovf(e); } }

    static long exI(int a, int b) {
        long h = 23;
        h = m(h, addX(a, b)); h = m(h, subX(a, b)); h = m(h, mulX(a, b));
        h = m(h, negX(a)); h = m(h, absX(a)); h = m(h, incX(a)); h = m(h, decX(a));
        // Divisor 0 is excluded: `/ by zero` is an implicit exception whose
        // message HotSpot may omit once hot (OmitStackTraceInFastThrow).
        int bb = b == 0 ? 1 : b;
        h = m(h, divX(a, bb)); h = m(h, fdivX(a, bb)); h = m(h, cdivX(a, bb));
        h = m(h, mulX(a, 3)); h = m(h, addX(a, Integer.MAX_VALUE)); h = m(h, subX(a, Integer.MIN_VALUE));
        return h;
    }

    // ---- long families ----------------------------------------------------

    static long divL(long a, long b) {
        long h = 29;
        h = m(h, a / 7L); h = m(h, a % 7L);
        h = m(h, a / -7L); h = m(h, a % -7L);
        h = m(h, a / 8L); h = m(h, a % 8L);
        h = m(h, a / -1L); h = m(h, a % -1L);
        h = m(h, a / 10L); h = m(h, a % 10L);
        h = m(h, a / 1000000007L); h = m(h, a % 1000000007L);
        h = m(h, a / Long.MIN_VALUE); h = m(h, a % Long.MIN_VALUE);
        h = m(h, a / Long.MAX_VALUE); h = m(h, a % 0x100000000L);
        h = m(h, a / 0x4000000000000000L);
        if (b != 0) {
            h = m(h, a / b); h = m(h, a % b);
        }
        long bb = b == 0 ? 1 : b;
        h = m(h, Math.floorDiv(a, bb)); h = m(h, Math.floorMod(a, bb));
        int ib = (int) bb == 0 ? 1 : (int) bb;
        h = m(h, Math.floorMod(a, ib)); h = m(h, Math.floorDiv(a, ib));
        h = m(h, Math.floorMod(a, -3)); h = m(h, Math.ceilDiv(a, bb));
        h = m(h, Long.divideUnsigned(a, bb)); h = m(h, Long.remainderUnsigned(a, bb));
        h = m(h, Long.divideUnsigned(a, 10L)); h = m(h, Long.remainderUnsigned(a, -2L));
        return h;
    }

    static long shL(long a, long b) {
        long h = 31;
        int s = (int) b;
        h = m(h, a << s); h = m(h, a >> s); h = m(h, a >>> s);
        h = m(h, a << b); h = m(h, a >>> b);
        h = m(h, a << 63); h = m(h, a >> 63); h = m(h, a >>> 63);
        h = m(h, a << 64); h = m(h, a >> 65); h = m(h, a >>> -1);
        h = m(h, a << 32); h = m(h, a >>> 32); h = m(h, (int) (a >>> 32));
        h = m(h, (a << s) >> s);
        h = m(h, a * 3L); h = m(h, a * -1L); h = m(h, a * 9L); h = m(h, a * 0x100000001L);
        h = m(h, a * Long.MIN_VALUE); h = m(h, a * Long.MAX_VALUE); h = m(h, a * b);
        h = m(h, Math.multiplyHigh(a, b)); h = m(h, Math.unsignedMultiplyHigh(a, b));
        h = m(h, Math.multiplyHigh(a, Long.MIN_VALUE)); h = m(h, Math.multiplyHigh(a, -1L));
        h = m(h, (int) a); h = m(h, (short) a); h = m(h, (char) a); h = m(h, (byte) a);
        h = m(h, (int) (a * 31L)); h = m(h, (long) (int) a);
        h = m(h, Long.rotateLeft(a, s)); h = m(h, Long.rotateRight(a, -s));
        h = m(h, Long.reverse(a)); h = m(h, Long.reverseBytes(a)); h = m(h, Long.bitCount(a));
        h = m(h, Long.numberOfLeadingZeros(a)); h = m(h, Long.numberOfTrailingZeros(a));
        h = m(h, Long.highestOneBit(a)); h = m(h, Long.lowestOneBit(a)); h = m(h, Long.signum(a));
        h = m(h, Long.compress(a, b)); h = m(h, Long.expand(a, b));
        h = m(h, Long.compareUnsigned(a, b)); h = m(h, Long.compare(a, b));
        h = m(h, Long.compareUnsigned(a, b) < 0 ? 1 : 0); h = m(h, a + Long.MIN_VALUE < b + Long.MIN_VALUE ? 1 : 0);
        h = m(h, Math.abs(a)); h = m(h, Math.max(a, b)); h = m(h, Math.min(a, b));
        h = m(h, a ^ (a >> 63)); h = m(h, a & -a); h = m(h, ~a);
        return h;
    }

    static long ovfL(ArithmeticException e) {
        String msg = e.getMessage();
        return 0x0BAD0BAD0BADL ^ (msg == null ? 0 : msg.hashCode());
    }

    static long addXL(long a, long b) { try { return Math.addExact(a, b); } catch (ArithmeticException e) { return ovfL(e); } }
    static long subXL(long a, long b) { try { return Math.subtractExact(a, b); } catch (ArithmeticException e) { return ovfL(e); } }
    static long mulXL(long a, long b) { try { return Math.multiplyExact(a, b); } catch (ArithmeticException e) { return ovfL(e); } }
    static long mulXLI(long a, int b) { try { return Math.multiplyExact(a, b); } catch (ArithmeticException e) { return ovfL(e); } }
    static long negXL(long a) { try { return Math.negateExact(a); } catch (ArithmeticException e) { return ovfL(e); } }
    static long absXL(long a) { try { return Math.absExact(a); } catch (ArithmeticException e) { return ovfL(e); } }
    static long toIntX(long a) { try { return Math.toIntExact(a); } catch (ArithmeticException e) { return ovfL(e); } }
    static long divXL(long a, long b) { try { return Math.divideExact(a, b); } catch (ArithmeticException e) { return ovfL(e); } }

    static long exL(long a, long b) {
        long h = 37;
        h = m(h, addXL(a, b)); h = m(h, subXL(a, b)); h = m(h, mulXL(a, b));
        h = m(h, mulXLI(a, (int) b)); h = m(h, negXL(a)); h = m(h, absXL(a));
        h = m(h, toIntX(a)); h = m(h, divXL(a, b == 0 ? 1L : b)); h = m(h, mulXL(a, 3L));
        return h;
    }

    // ---- input sources (the deopt trigger) --------------------------------

    abstract static class Src {
        abstract int a(int i);
        abstract int b(int i);
        abstract long la(int i);
        abstract long lb(int i);
    }

    static final class Lcg extends Src {
        int a(int i) {
            return (i & 1) == 0 ? IV[(i >>> 1) % IV.length] : i * 0x9E3779B1;
        }

        int b(int i) {
            if ((i & 4095) == 4095) {
                return 0;
            }
            return (i & 2) == 0 ? IV[(i >>> 2) % IV.length] : (i ^ (i >>> 5)) * 0x85EBCA6B;
        }

        long la(int i) {
            return (i & 1) == 0 ? LV[(i >>> 1) % LV.length] : i * 0x9E3779B97F4A7C15L;
        }

        long lb(int i) {
            if ((i & 4095) == 2047) {
                return 0L;
            }
            return (i & 2) == 0 ? LV[(i >>> 2) % LV.length] : ((long) i << 29) ^ (i * 0xC2B2AE3D27D4EB4FL);
        }
    }

    // Never touched before the `late` pass.
    static final class Late extends Src {
        int a(int i) {
            return (i % 3) == 0 ? Integer.MIN_VALUE : ~IV[i % IV.length];
        }

        int b(int i) {
            return (i % 5) == 0 ? -1 : ((i % 7) == 0 ? 0 : IV[(i * 3) % IV.length]);
        }

        long la(int i) {
            return (i % 3) == 0 ? Long.MIN_VALUE : ~LV[i % LV.length];
        }

        long lb(int i) {
            return (i % 5) == 0 ? -1L : ((i % 7) == 0 ? 0L : LV[(i * 3) % LV.length]);
        }
    }

    static long hotI(Src s, int n) {
        long h = 0;
        int exc = 0;
        for (int i = 0; i < n; i++) {
            int a = s.a(i);
            int b = s.b(i);
            h = m(h, divI(a, b));
            h = m(h, shI(a, b));
            h = m(h, exI(a, b));
            try {
                h = m(h, a / b);
            } catch (ArithmeticException e) {
                exc++;
            }
            try {
                h = m(h, a % b);
            } catch (ArithmeticException e) {
                exc++;
            }
        }
        excCount = exc;
        return h;
    }

    static long hotL(Src s, int n) {
        long h = 0;
        int exc = 0;
        for (int i = 0; i < n; i++) {
            long a = s.la(i);
            long b = s.lb(i);
            h = m(h, divL(a, b));
            h = m(h, shL(a, b));
            h = m(h, exL(a, b));
            try {
                h = m(h, a / b);
            } catch (ArithmeticException e) {
                exc++;
            }
            try {
                h = m(h, a % b);
            } catch (ArithmeticException e) {
                exc++;
            }
        }
        excCount = exc;
        return h;
    }

    static void tables(String tag) {
        for (int a : IV) {
            long hd = 0, hs = 0, he = 0;
            for (int b : IV) {
                hd = m(hd, divI(a, b));
                hs = m(hs, shI(a, b));
                he = m(he, exI(a, b));
            }
            System.out.println(tag + " int a=" + a + " div=" + hx(hd) + " sh=" + hx(hs) + " ex=" + hx(he));
        }
        for (long a : LV) {
            long hd = 0, hs = 0, he = 0;
            for (long b : LV) {
                hd = m(hd, divL(a, b));
                hs = m(hs, shL(a, b));
                he = m(he, exL(a, b));
            }
            System.out.println(tag + " long a=" + a + " div=" + hx(hd) + " sh=" + hx(hs) + " ex=" + hx(he));
        }
    }

    static String exText(Runnable r) {
        try {
            r.run();
            return "none";
        } catch (ArithmeticException e) {
            return e.getClass().getName() + ":" + e.getMessage();
        }
    }

    static int sinkI;
    static long sinkL;

    static void edges(String tag) {
        int min = Integer.MIN_VALUE;
        long lmin = Long.MIN_VALUE;
        int m1 = -1;
        long lm1 = -1L;
        System.out.println(tag + " MIN/-1 div=" + (min / m1) + " rem=" + (min % m1)
            + " floorDiv=" + Math.floorDiv(min, m1) + " floorMod=" + Math.floorMod(min, m1)
            + " ceilDiv=" + Math.ceilDiv(min, m1) + " udiv=" + Integer.divideUnsigned(min, m1)
            + " urem=" + Integer.remainderUnsigned(min, m1));
        System.out.println(tag + " LMIN/-1 div=" + (lmin / lm1) + " rem=" + (lmin % lm1)
            + " floorDiv=" + Math.floorDiv(lmin, lm1) + " floorMod=" + Math.floorMod(lmin, lm1)
            + " floorModLI=" + Math.floorMod(lmin, m1) + " udiv=" + Long.divideUnsigned(lmin, lm1));
        System.out.println(tag + " floorMod(-7,3)=" + Math.floorMod(-7, 3) + " floorMod(7,-3)=" + Math.floorMod(7, -3)
            + " floorDiv(-7,2)=" + Math.floorDiv(-7, 2) + " -7/2=" + (-7 / 2) + " -7%2=" + (-7 % 2)
            + " ceilDiv(7,2)=" + Math.ceilDiv(7, 2) + " ceilMod(7,2)=" + Math.ceilMod(7, 2));
        System.out.println(tag + " mulHigh(MIN,MIN)=" + Math.multiplyHigh(lmin, lmin)
            + " mulHigh(MAX,MAX)=" + Math.multiplyHigh(Long.MAX_VALUE, Long.MAX_VALUE)
            + " umulHigh(-1,-1)=" + Math.unsignedMultiplyHigh(-1L, -1L)
            + " umulHigh(MIN,2)=" + Math.unsignedMultiplyHigh(lmin, 2L)
            + " mulFull(MIN,MIN)=" + Math.multiplyFull(min, min));
        System.out.println(tag + " exact absI " + exText(() -> sinkI = Math.absExact(Integer.MIN_VALUE)));
        System.out.println(tag + " exact absL " + exText(() -> sinkL = Math.absExact(Long.MIN_VALUE)));
        System.out.println(tag + " exact negI " + exText(() -> sinkI = Math.negateExact(Integer.MIN_VALUE)));
        System.out.println(tag + " exact addI " + exText(() -> sinkI = Math.addExact(Integer.MAX_VALUE, 1)));
        System.out.println(tag + " exact mulL " + exText(() -> sinkL = Math.multiplyExact(3037000500L, 3037000500L)));
        System.out.println(tag + " exact mulLI " + exText(() -> sinkL = Math.multiplyExact(Long.MIN_VALUE, -1)));
        System.out.println(tag + " exact toInt " + exText(() -> sinkI = Math.toIntExact(0x80000000L)));
        System.out.println(tag + " exact divI " + exText(() -> sinkI = Math.divideExact(Integer.MIN_VALUE, -1)));
        System.out.println(tag + " exact floorDivL " + exText(() -> sinkL = Math.floorDivExact(Long.MIN_VALUE, -1L)));
        System.out.println(tag + " exact incI " + exText(() -> sinkI = Math.incrementExact(Integer.MAX_VALUE)));
        System.out.println(tag + " exact decL " + exText(() -> sinkL = Math.decrementExact(Long.MIN_VALUE)));
        int one = 1;
        System.out.println(tag + " shifts 1<<32=" + (one << 32) + " 1<<33=" + (one << 33) + " -1>>>32=" + (m1 >>> 32)
            + " -1>>>33=" + (m1 >>> 33) + " 1L<<64=" + (1L << 64) + " 1L<<65=" + (1L << 65) + " -1L>>>64=" + (lm1 >>> 64)
            + " MIN>>31=" + (min >> 31) + " MIN>>>31=" + (min >>> 31));
        System.out.println(tag + " unsigned cmp(-1,1)=" + Integer.compareUnsigned(-1, 1)
            + " cmpL(MIN,MAX)=" + Long.compareUnsigned(lmin, Long.MAX_VALUE)
            + " udivL(-1,10)=" + Long.divideUnsigned(-1L, 10L) + " uremL(-1,10)=" + Long.remainderUnsigned(-1L, 10L)
            + " ustrI=" + Integer.toUnsignedString(-1) + " ustrL=" + Long.toUnsignedString(lmin));
        System.out.println(tag + " compress=" + Integer.compress(0xCAFEBABE, 0xFF00FF00) + " expand="
            + Integer.expand(0xCAFEBABE, 0x0F0F0F0F) + " compressL=" + Long.compress(-1L, 0x8000000000000001L)
            + " expandL=" + Long.expand(0x3L, 0x8000000000000001L));
        try {
            int z = sinkI * 0;
            sinkI = 5 / z;
            System.out.println(tag + " div0 none");
        } catch (ArithmeticException e) {
            System.out.println(tag + " div0 " + e.getClass().getName());
        }
        try {
            long z = sinkL * 0;
            sinkL = 5L % z;
            System.out.println(tag + " rem0L none");
        } catch (ArithmeticException e) {
            System.out.println(tag + " rem0L " + e.getClass().getName());
        }
    }

    public static void main(String[] args) {
        tables("cold");
        edges("cold");
        Src lcg = new Lcg();
        for (int pass = 0; pass < 3; pass++) {
            long hi = hotI(lcg, HOT);
            int ei = excCount;
            long hl = hotL(lcg, HOT);
            int el = excCount;
            System.out.println("hot" + pass + " int h=" + hx(hi) + " exc=" + ei + " long h=" + hx(hl) + " exc=" + el);
        }
        Src late = new Late();
        long hi = hotI(late, HOT / 4);
        int ei = excCount;
        long hl = hotL(late, HOT / 4);
        int el = excCount;
        System.out.println("late int h=" + hx(hi) + " exc=" + ei + " long h=" + hx(hl) + " exc=" + el);
        for (int pass = 0; pass < 2; pass++) {
            hi = hotI(lcg, HOT);
            ei = excCount;
            hl = hotL(lcg, HOT);
            el = excCount;
            System.out.println("rehot" + pass + " int h=" + hx(hi) + " exc=" + ei + " long h=" + hx(hl) + " exc=" + el);
        }
        tables("after");
        edges("after");
        System.out.println("done");
    }
}
