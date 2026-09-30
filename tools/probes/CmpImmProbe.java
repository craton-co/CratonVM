/**
 * Differential check against HotSpot for `CRATONVM_JIT_IR_CMP_IN_PLACE`'s
 * immediate forms and for `CRATONVM_JIT_IR_ADD_LEA`.
 *
 * <p>The forms this covers are wrong in ways that do not fault, which is what
 * makes a differential probe worth more than a crash test:
 *
 * <ul>
 *   <li>the `imm8`/`imm32` boundary — a comparison against 127 encodes in three
 *       bytes and one against 128 in six, and the two must agree;</li>
 *   <li>NEGATIVE constants, where `83 /7 ib` sign-extends: `i &gt; -1` must not
 *       become `i &gt; 255`;</li>
 *   <li>a `long` constant outside `i32`, which no immediate here can express,
 *       so the comparison has to fall back to the register form rather than
 *       compare against a truncated one;</li>
 *   <li>`Integer.MIN_VALUE` and `Long.MIN_VALUE` as bounds and as addends —
 *       `x - Integer.MIN_VALUE` reaches the `LEA` path with a displacement
 *       whose negation is not an `int`, and must decline rather than wrap;</li>
 *   <li>the FIRST operand in a frame slot rather than a register, which is the
 *       `cmp [rbp-x], imm` form — `spilled` runs enough live values to push the
 *       counter out of the five-register file;</li>
 *   <li>a reference against `null`, which takes the 64-bit form of the same
 *       encoding.</li>
 * </ul>
 *
 * <p>Kept in its own methods, called in a loop, for the reason `OsrTierProbe`
 * records: a `main` that formats its output disqualifies itself from the
 * optimizing tier, and a kernel inlined into it would be disqualified with it.
 */
public class CmpImmProbe {

    /** Every `imm8`/`imm32` boundary and sign, as loop bounds and as tests. */
    static int bounds(int x) {
        int a = 0;
        for (int i = 0; i < 127; i++) {
            a += i;
        }
        for (int i = 0; i < 128; i++) {
            a ^= i;
        }
        for (int i = 0; i < 4096; i++) {
            a += i & 3;
        }
        for (int i = -128; i < 0; i++) {
            a -= i;
        }
        for (int i = 20000; i > 0; i--) {
            a |= i & 7;
        }
        if (x > -1) {
            a += 11;
        }
        if (x >= -129) {
            a += 13;
        }
        if (x < Integer.MAX_VALUE) {
            a += 17;
        }
        if (x > Integer.MIN_VALUE) {
            a += 19;
        }
        if (x == 0) {
            a += 23;
        }
        if (x != 100) {
            a += 29;
        }
        return a;
    }

    /** The `long` half: a constant inside `i32`, and one that is not. */
    static long wideBounds(long x) {
        long a = 0;
        if (x < 100L) {
            a += 3;
        }
        // Outside `i32`: no immediate form sign-extends to this, so the
        // comparison must go through a register rather than truncate.
        if (x < 0xFFFFFFFFL) {
            a += 5;
        }
        if (x > Long.MIN_VALUE) {
            a += 7;
        }
        if (x <= 4294967296L) {
            a += 11;
        }
        for (long i = 0; i < 100L; i++) {
            a += i;
        }
        return a;
    }

    /** The `LEA` path's constants, including the one whose negation is not one. */
    static int addends(int x) {
        int a = x + 1;
        a = a - 1;
        a = a + 127;
        a = a - 128;
        a = a + 4096;
        a = a - 4096;
        a = a + Integer.MAX_VALUE;
        // `x - Integer.MIN_VALUE` is `x + (-MIN)`, and `-MIN` is not an `int`.
        // The lowering must decline the fold here, not wrap it.
        a = a - Integer.MIN_VALUE;
        a = a + Integer.MIN_VALUE;
        a = a + 0;
        return a;
    }

    static long wideAddends(long x) {
        long a = x + 1L;
        a = a - 1L;
        a = a + 2147483647L;
        // Outside `i32` in both directions.
        a = a + 4294967296L;
        a = a - 4294967296L;
        a = a - Long.MIN_VALUE;
        a = a + Long.MIN_VALUE;
        return a;
    }

    /**
     * Enough simultaneously live values that the loop counter loses its
     * register, which is what puts the comparison in the `cmp [rbp-x], imm`
     * form rather than the register one.
     */
    static int spilled(int x) {
        int a = x, b = x + 1, c = x + 2, d = x + 3, e = x + 4;
        int f = x + 5, g = x + 6, h = x + 7, j = x + 8, k = x + 9;
        for (int i = 0; i < 300; i++) {
            a ^= i;
            b += a;
            c |= b;
            d -= c;
            e ^= d;
            f += e;
            g |= f;
            h -= g;
            j ^= h;
            k += j;
        }
        return a + b + c + d + e + f + g + h + j + k;
    }

    /** The 64-bit form of the same encoding: a reference against `null`. */
    static int refs(Object o, int n) {
        int a = 0;
        for (int i = 0; i < n; i++) {
            if (o == null) {
                a += 1;
            } else {
                a += 2;
            }
        }
        return a;
    }

    static long run(int n) {
        long acc = 0;
        Object o = new Object();
        for (int i = 0; i < n; i++) {
            acc += bounds(i - 3);
            acc ^= wideBounds(i * 2654435761L);
            acc += addends(i);
            acc ^= wideAddends(i);
            acc += spilled(i);
            acc += refs(i % 2 == 0 ? o : null, 4);
        }
        return acc;
    }

    public static void main(String[] args) {
        int n = Integer.getInteger("probe.n", 200_000);
        long ck = run(n);
        System.out.print("CMPIMM ck=");
        System.out.println(ck);
    }
}
