// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe + bench
//
// Interpreter round i6 wave 115, lane L7: `CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES` (default
// off; the last open item of
// docs/internal/proposals/interpreter/i24-L4-proposal-frameless-trivial-constructor-in-the-special-door-20260927.md).
// The special door's frameless field-store constructor (wave 28) now also answers a
// constructor with a `long` / `double` parameter and `lconst` / `dconst` stores, mapping the
// body's locals to arguments by the descriptor's slot widths and storing a category-2 value with
// its kind mark. Every row builds objects through such constructors in a loop (so the field
// sites are filled and the door answers them) and prints the fields, bit-exact:
//
//   span        Span(long start, long end)                     lload_1, lload_3
//   vec2        Vec2(double x, double y)                       dload_1, dload_3
//   entry       Entry(int key, long value, Object tag)         iload_1, lload_2, aload 4
//   mixed       Mixed(double d, int i, long l) + long k = 1L   dload_1, iload_3, lload 4, lconst_1
//   consts      Consts() { a = 0L; b = 1L; c = 0.0; d = 1.0 }  lconst_0/1, dconst_0/1
//   nan-payload Vec2(nanWithPayload, -0.0)                     raw bits kept (doubleToRawLongBits)
//   extremes    Span(Long.MIN_VALUE, Long.MAX_VALUE)
//   subclass    new SubSpan(a, b) -> super(a, b)               a subclass receiver
//   finals      FinalSpan(long a, long b), final fields
//   narrow      IntPair(int a, int b) (control: the wave-28 shape, unchanged)
//   gc-tags     Entry tags survive System.gc() between constructions
//
// Bench (TIME lines): `new-span`, `new-vec2`, `new-entry` expected down with the switch
// (`--nojit`; with the JIT these allocations are compiled); `new-intpair` flat (control).
//
// Run, switch off and on, --jdk-only and --compatible, with and without --nojit:
//   cratonvm --java-home <jdk25> --jdk-only --nojit -cp <dir> L7W115FieldCtorWideValues
//   CRATONVM_INTERP_FIELD_CTOR_WIDE_VALUES=1 cratonvm --java-home <jdk25> --jdk-only --nojit -cp <dir> L7W115FieldCtorWideValues
// Census: CRATONVM_DBG_FIELD_SITE=1 prints `[invoke-door] field-store constructors: elided=N
// framed=M`; with the switch, `elided` should grow by the rows' constructions.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// span 14997000000000 -4999 acc=-37492500001229404
// vec2 2499.5 -1666.3333333333333 acc=2082916.6666666667
// entry 4999 42941083025408 4999 acc=107352715396092500
// mixed 4999.25 4999 -5496458627252224 1 acc=-1.3741146568105548E19
// consts 0 1 0.0 1.0
// nan-payload fffc000012345678 8000000000000000
// extremes -9223372036854775808 9223372036854775807
// subclass SubSpan 4999 -34993 acc=99980000
// finals 4999 5000 acc=41666665000
// narrow 4999 -4999 acc=24995000
// gc-tags 4992:4992:t4992 4993:4993:t4993 4994:4994:t4994 4995:4995:t4995
// new-span checksum=125000250000
// new-vec2 checksum=500000
// new-entry checksum=124999750000
// new-intpair checksum=500000
// HOTSPOT_EXPECTED_END

public class L7W115FieldCtorWideValues {
    static final class Span {
        long start;
        long end;

        Span(long start, long end) {
            this.start = start;
            this.end = end;
        }
    }

    static final class Vec2 {
        double x;
        double y;

        Vec2(double x, double y) {
            this.x = x;
            this.y = y;
        }
    }

    static final class Entry {
        int key;
        long value;
        Object tag;

        Entry(int key, long value, Object tag) {
            this.key = key;
            this.value = value;
            this.tag = tag;
        }
    }

    static final class Mixed {
        double d;
        int i;
        long l;
        long k = 1L;

        Mixed(double d, int i, long l) {
            this.d = d;
            this.i = i;
            this.l = l;
        }
    }

    static final class Consts {
        long a = 0L;
        long b = 1L;
        double c = 0.0;
        double d = 1.0;
    }

    static class BaseSpan {
        long lo;
        long hi;

        BaseSpan(long lo, long hi) {
            this.lo = lo;
            this.hi = hi;
        }
    }

    static final class SubSpan extends BaseSpan {
        SubSpan(long lo, long hi) {
            super(lo, hi);
        }
    }

    static final class FinalSpan {
        final long a;
        final long b;

        FinalSpan(long a, long b) {
            this.a = a;
            this.b = b;
        }
    }

    static final class IntPair {
        int a;
        int b;

        IntPair(int a, int b) {
            this.a = a;
            this.b = b;
        }
    }

    static final int ROUNDS = 5_000;

    public static void main(String[] args) {
        semantics();
        bench(args.length > 0 ? args[0] : null);
    }

    static void semantics() {
        long acc = 0;
        Span s = null;
        for (int i = 0; i < ROUNDS; i++) {
            s = new Span(i * 3_000_000_000L, -i);
            acc += s.start ^ s.end;
        }
        System.out.println("span " + s.start + " " + s.end + " acc=" + acc);

        double dacc = 0;
        Vec2 v = null;
        for (int i = 0; i < ROUNDS; i++) {
            v = new Vec2(i * 0.5, -i / 3.0);
            dacc += v.x + v.y;
        }
        System.out.println("vec2 " + v.x + " " + v.y + " acc=" + dacc);

        Entry e = null;
        acc = 0;
        for (int i = 0; i < ROUNDS; i++) {
            e = new Entry(i, (long) i << 33, (i & 1) == 0 ? "even" : Integer.valueOf(i));
            acc += e.key + e.value + e.tag.hashCode();
        }
        System.out.println("entry " + e.key + " " + e.value + " " + e.tag + " acc=" + acc);

        Mixed m = null;
        dacc = 0;
        for (int i = 0; i < ROUNDS; i++) {
            m = new Mixed(i + 0.25, i, -((long) i << 40));
            dacc += m.d + m.i + m.l + m.k;
        }
        System.out.println("mixed " + m.d + " " + m.i + " " + m.l + " " + m.k + " acc=" + dacc);

        Consts c = null;
        for (int i = 0; i < ROUNDS; i++) {
            c = new Consts();
        }
        System.out.println("consts " + c.a + " " + c.b + " " + c.c + " " + c.d);

        double nan = Double.longBitsToDouble(0xfffc_0000_1234_5678L);
        Vec2 n = null;
        for (int i = 0; i < ROUNDS; i++) {
            n = new Vec2(nan, -0.0);
        }
        System.out.println("nan-payload " + Long.toHexString(Double.doubleToRawLongBits(n.x)) + " "
                + Long.toHexString(Double.doubleToRawLongBits(n.y)));

        for (int i = 0; i < ROUNDS; i++) {
            s = new Span(Long.MIN_VALUE, Long.MAX_VALUE);
        }
        System.out.println("extremes " + s.start + " " + s.end);

        BaseSpan b = null;
        acc = 0;
        for (int i = 0; i < ROUNDS; i++) {
            b = new SubSpan(i, -i * 7L);
            acc += b.lo - b.hi;
        }
        System.out.println("subclass " + b.getClass().getSimpleName() + " " + b.lo + " " + b.hi + " acc=" + acc);

        FinalSpan f = null;
        acc = 0;
        for (int i = 0; i < ROUNDS; i++) {
            f = new FinalSpan(i, i + 1L);
            acc += f.a * f.b;
        }
        System.out.println("finals " + f.a + " " + f.b + " acc=" + acc);

        IntPair p = null;
        acc = 0;
        for (int i = 0; i < ROUNDS; i++) {
            p = new IntPair(i, -i);
            acc += p.a - p.b;
        }
        System.out.println("narrow " + p.a + " " + p.b + " acc=" + acc);

        Entry[] kept = new Entry[64];
        for (int i = 0; i < ROUNDS; i++) {
            kept[i & 63] = new Entry(i, i, new StringBuilder("t").append(i).toString());
            if ((i & 1023) == 0) {
                System.gc();
            }
        }
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 4; i++) {
            sb.append(kept[i].key).append(':').append(kept[i].value).append(':').append(kept[i].tag).append(' ');
        }
        System.out.println("gc-tags " + sb.toString().trim());
    }

    static long newSpan(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += new Span(i, i + 1L).end;
        }
        return s;
    }

    static long newVec2(int n) {
        double s = 0;
        for (int i = 0; i < n; i++) {
            s += new Vec2(i, 1.0).y;
        }
        return (long) s;
    }

    static long newEntry(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += new Entry(i, i, null).value;
        }
        return s;
    }

    static long newIntPair(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += new IntPair(i, 1).b;
        }
        return s;
    }

    interface Row {
        long run(int n);
    }

    static void bench(String only) {
        String[] names = {"new-span", "new-vec2", "new-entry", "new-intpair"};
        Row[] rows = {
            L7W115FieldCtorWideValues::newSpan,
            L7W115FieldCtorWideValues::newVec2,
            L7W115FieldCtorWideValues::newEntry,
            L7W115FieldCtorWideValues::newIntPair,
        };
        final int n = 500_000;
        for (int r = 0; r < rows.length; r++) {
            if (only != null && !only.equals(names[r])) {
                continue;
            }
            long checksum = rows[r].run(n);
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                long c = rows[r].run(n);
                best = Math.min(best, System.nanoTime() - t0);
                if (c != checksum) {
                    System.out.println(names[r] + " UNSTABLE");
                }
            }
            System.out.println(names[r] + " checksum=" + checksum);
            System.out.println("TIME " + names[r] + " " + (best / n) + " ns/op (min of 5)");
        }
    }
}
