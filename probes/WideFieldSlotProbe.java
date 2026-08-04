// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * COV-03 — a wide field value moving through category-2 LOCAL slots, and
 * through a deopt.
 *
 * <p>The lane brief flagged this and named the bug to be afraid of: *"they
 * carry the category-2 slot question, which the IR's parameter model has
 * already been burned by once — see the `optimize=false` guard's comment about
 * `boolean eq(long, long)` truncating its second parameter."* That defect was
 * the IR laying parameters out by JIT-ARGUMENT index while the bytecode reads
 * them by JVM LOCAL SLOT: a `long` occupies two slots, so the second `long`
 * parameter was read from a slot nothing had populated.
 *
 * <p>A field access is one value on the IR's operand stack whatever its width,
 * so in principle it cannot reintroduce that bug. In principle is not a test.
 * What a wide FIELD adds is a new *source* for a category-2 value, and every
 * consumer downstream of it — `lstore` into a two-slot local, a later local
 * whose slot index depends on this one being two wide, a deopt snapshot that
 * has to rebuild the interpreter's two-slot frame — is machinery that was
 * written and tested when the only sources were parameters, constants and
 * calls.
 *
 * <p>So each method below deliberately puts a wide field value NEXT TO other
 * category-2 values and reads them all back. If any slot layout is off by the
 * one slot a category-2 value occupies, a neighbour returns garbage rather
 * than faulting.
 *
 * <pre>
 *   CRATONVM_JIT=force-c2 CRATONVM_DBG=ir-compiles \
 *     cratonvm -cp probes WideFieldSlotProbe
 * </pre>
 */
public class WideFieldSlotProbe {

    static final class Box {
        long l;
        double d;
        float f;
        int guard;
    }

    /**
     * The `eq(long, long)` shape, with a wide FIELD in the middle.
     *
     * <p>Slots: `b`=0, `a`=1..2, `c`=3..4, then `x`=5..6 and `y`=7..8. Every
     * one of those indices is only correct if each preceding category-2 value
     * consumed two slots. `c` is the parameter the original defect truncated.
     */
    static long mixLong(Box b, long a, long c) {
        long x = b.l;          // getfield J -> a two-slot local
        long y = a + c;        // the two long params, read by slot
        b.l = x + y;           // putfield J
        return b.l - x;        // == y, iff every slot above is right
    }

    /** Same shape for `double`, whose slot arithmetic is identical. */
    static double mixDouble(Box b, double a, double c) {
        double x = b.d;
        double y = a + c;
        b.d = x + y;
        return b.d - x;
    }

    /**
     * `float` is category-ONE, so it must NOT consume two slots. A layout that
     * over-counts is as wrong as one that under-counts, and only a mixed
     * signature catches it: `f1`=1, `n`=2, `f2`=3.
     */
    static float mixFloat(Box b, float f1, int n, float f2) {
        float x = b.f;
        b.f = x + f1 + f2 + n;
        return b.f - x;
    }

    /**
     * A wide field value live across a div-by-zero GUARD, in a body that is
     * actually compiled.
     *
     * <p>NO try/catch here, deliberately. The first draft caught the
     * `ArithmeticException` in this method so the interpreter would resume at
     * the handler and the rebuilt `long` could be read back — the case the
     * admission chain's own comment describes ("a `long` live at an `idiv`
     * deopt"). That method was refused by **both** backends before the
     * admission chain ever ran, at `rbc6-handler-reads-unsafe-local`: the code
     * after the handler reads `x`, a non-parameter local. So the one shape that
     * would let Java observe the reconstructed frame is structurally
     * uncompilable, and a probe written that way reports PASS while testing the
     * interpreter. See this lane's closeout — it is named there as a residual
     * rather than papered over.
     *
     * <p>What this version does establish, in a C2 body: a `long` sourced from
     * a field is correct on the guard's fall-through path, the guard still
     * fires on `d == 0` rather than returning a fabricated value, and the body
     * is still correct afterwards.
     */
    static long divGuard(Box b, int n, int d) {
        long x = b.l;
        return x + (n / d);
    }

    private static int check(String what, long got, long want, int failures) {
        if (got != want) {
            System.out.println("[wideslot] FAIL " + what + ": got " + got + " want " + want);
            return failures + 1;
        }
        return failures;
    }

    public static void main(String[] args) {
        final int warm = Integer.getInteger("probe.warm", 300000);
        Box b = new Box();
        b.guard = 0x5A5A5A5A;
        int failures = 0;

        // Warm every method past the C2 threshold.
        //
        // FP values are POWERS OF TWO, and the arithmetic on them is exactly
        // representable, so each expected constant is provable rather than
        // approximately right. The first draft of this probe used `1.5e20f` as
        // the float base and expected `+7` to change it; one ULP up there is
        // ~1e13, so the answer was 0.0f and the probe reported a VM failure
        // that was its own arithmetic. An expectation you cannot derive by hand
        // is not an oracle.
        for (int i = 1; i <= warm; i++) {
            b.l = 0x1234_5678_9ABCL + i;
            b.d = 4.0d;
            b.f = 4.0f;
            long r = mixLong(b, 0x7FFF_FFFF_0000_0001L, 0x0000_0002_0000_0003L);
            if (r != 0x8000_0001_0000_0004L) {
                failures = check("mixLong warm i=" + i, r, 0x8000_0001_0000_0004L, failures);
                break;
            }
            double dr = mixDouble(b, 1.0d, 2.0d);
            if (dr != 3.0d) {
                System.out.println("[wideslot] FAIL mixDouble warm i=" + i + ": " + dr);
                failures++;
                break;
            }
            float fr = mixFloat(b, 1.0f, 2, 4.0f);
            if (fr != 7.0f) {
                System.out.println("[wideslot] FAIL mixFloat warm i=" + i + ": " + fr);
                failures++;
                break;
            }
            failures = check("divGuard warm i=" + i, divGuard(b, 10, 5), b.l + 2, failures);
            if (failures != 0) {
                break;
            }
        }

        // The values a truncation to 32 bits would destroy, once compiled.
        b.l = Long.MIN_VALUE;
        failures = check("mixLong MIN", mixLong(b, 1L, 2L), 3L, failures);
        b.l = -1L;
        failures = check("mixLong -1", mixLong(b, Long.MAX_VALUE, 1L), Long.MIN_VALUE, failures);
        b.l = 0x0000_0001_0000_0000L;
        failures = check("mixLong hi32", mixLong(b, 0x0000_0004_0000_0000L, 0L),
                0x0000_0004_0000_0000L, failures);

        b.d = -0.0d;
        double dr = mixDouble(b, 1.0d, 2.0d);
        if (dr != 3.0d) {
            System.out.println("[wideslot] FAIL mixDouble -0.0 base: " + dr);
            failures++;
        }
        b.f = -0.0f;
        float fr = mixFloat(b, 1.0f, 2, 4.0f);
        if (fr != 7.0f) {
            System.out.println("[wideslot] FAIL mixFloat -0.0 base: " + fr);
            failures++;
        }

        // The high bits, in the only form where the expected answer is exact:
        // 2^1000 + 2^1001 == 3 * 2^1000 needs two significand bits and a
        // 64-bit exponent field, so a value truncated or rebuilt through 32
        // bits cannot produce it. Same shape at 2^100 for float.
        b.d = 0x1p1000;
        dr = mixDouble(b, 0x1p1000, 0x1p1000);
        if (dr != 0x1p1001) {
            System.out.println("[wideslot] FAIL mixDouble 2^1000: " + dr);
            failures++;
        }
        b.f = 0x1p100f;
        fr = mixFloat(b, 0x1p100f, 0, 0x1p100f);
        if (fr != 0x1p101f) {
            System.out.println("[wideslot] FAIL mixFloat 2^100: " + fr);
            failures++;
        }

        // The guard, in a compiled body, with a field-sourced long live across
        // it. It must THROW — a guard that returned a fabricated value here is
        // the silent-corruption shape, and it would look identical to success.
        b.l = Long.MIN_VALUE;
        failures = check("divGuard MIN fallthrough", divGuard(b, 4, 2), Long.MIN_VALUE + 2, failures);
        boolean threw = false;
        try {
            divGuard(b, 1, 0);
        } catch (ArithmeticException expected) {
            threw = true;
        }
        if (!threw) {
            System.out.println("[wideslot] FAIL divGuard(d=0) did not throw");
            failures++;
        }
        // …and the body is still right after the guard fired.
        b.l = 0x7FFF_FFFF_FFFF_FFFFL;
        failures = check("divGuard MAX after deopt", divGuard(b, 4, 2),
                0x7FFF_FFFF_FFFF_FFFFL + 2, failures);

        if (b.guard != 0x5A5A5A5A) {
            System.out.println("[wideslot] FAIL neighbouring field clobbered: "
                    + Integer.toHexString(b.guard));
            failures++;
        }

        System.out.println("[wideslot] failures=" + failures);
        System.out.println(failures == 0 ? "[wideslot] PASS" : "[wideslot] FAIL");
        if (failures != 0) {
            System.exit(1);
        }
    }
}
