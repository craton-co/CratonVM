/**
 * Differential check for `CRATONVM_JIT_IR_ALU_IMM`: every arithmetic arm that
 * can fold a constant second operand, against HotSpot.
 *
 * The interesting cases are the ones a naive immediate form gets wrong:
 *
 * <ul>
 *   <li>a shift count outside the width — the JVM masks `ishl` to 5 bits and
 *       `lshl` to 6, so `x << 32` is `x` for an int and `x << 64` is `x` for a
 *       long. x86 masks the same way, which is what makes the register (CL)
 *       form correct without a mask; the immediate form must not depend on
 *       that coincidence, so it masks explicitly and this checks it;</li>
 *   <li>a NEGATIVE shift count, which masks to a large positive one;</li>
 *   <li>a long constant outside `i32`, where every immediate form's
 *       sign-extension cannot express it and the lowering must fall back to
 *       the register form — `0xFFFFFFFFL` is the case that separates a
 *       correct fallback from a truncating one;</li>
 *   <li>`Integer.MIN_VALUE` as an operand and as a dividend, where the
 *       two's-complement edge is its own arm.</li>
 * </ul>
 *
 * Kept in its own method, called in a loop, for the reason `OsrTierProbe`
 * records: a `main` that formats its output disqualifies itself from the
 * optimizing tier, and a kernel inlined into it would be disqualified with it.
 */
public class AluImmProbe {

    static int kernelInt(int x) {
        int a = x + 1;
        a = a - 7;
        a = a * 31;
        a = a & 0xFF;
        a = a | 0x1000;
        a = a ^ 0x5A5A;
        a = a << 3;
        a = a >> 2;
        a = a >>> 1;
        // Counts the JVM masks: 32 is a no-op, -1 is a shift by 31.
        a = a << 32;
        a = a >>> 32;
        a = a >> 32;
        a = a << -1;
        a = a + Integer.MIN_VALUE;
        a = a * -1;
        a = a - Integer.MAX_VALUE;
        return a;
    }

    static long kernelLong(long x) {
        long a = x + 1L;
        a = a - 7L;
        a = a * 1000003L;
        a = a & 0xFFL;
        // Outside i32: no immediate form can express it, so the lowering must
        // fall back to the register form rather than truncate.
        a = a & 0xFFFFFFFFL;
        a = a | 0x100000000L;
        a = a ^ 0x5A5A5A5A5AL;
        a = a << 3;
        a = a >> 2;
        a = a >>> 1;
        a = a << 64;
        a = a >>> 64;
        a = a >> 64;
        a = a << -1;
        a = a + Long.MIN_VALUE;
        a = a * -1L;
        return a;
    }

    static long run(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += kernelInt(i);
            acc ^= kernelLong(i * 2654435761L);
        }
        return acc;
    }

    public static void main(String[] args) {
        int n = Integer.getInteger("probe.n", 2_000_000);
        long ck = run(n);
        System.out.print("ALU ck=");
        System.out.println(ck);
    }
}
