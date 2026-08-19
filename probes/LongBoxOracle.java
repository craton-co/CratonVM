/**
 * Correctness pin for the `Long.valueOf(J)` / `Long.longValue()` thin
 * direct-call helpers (`jit_long_value_of_direct` /
 * `jit_long_long_value_direct`).
 *
 * The helpers reimplement, in the VM, what a registered native does — so the
 * question is not "is it fast" but "does it still agree". This probe must print
 * **byte-for-byte the same output on HotSpot and on CratonVM**, and the same
 * output with `CRATONVM_JIT_LONG_BOX_DIRECT_HELPERS=0` as with it on. Three
 * arms, one file:
 *
 *   java @common-ish            -cp out LongBoxOracle          # oracle
 *   cratonvm ... -cp out LongBoxOracle                         # binds on
 *   CRATONVM_JIT_LONG_BOX_DIRECT_HELPERS=0 cratonvm ... LongBoxOracle
 *
 * What it pins, and why each row is here rather than a round number:
 *
 *  * **The JLS identity cache.** `Long.valueOf` must return the SAME object for
 *    every value in `-128..=127` and a fresh one outside it. The fast path
 *    allocates directly out of the TLAB and would happily mint a second
 *    wrapper for `127L` if the range test were wrong by one — which is exactly
 *    the boundary printed below.
 *  * **The full 64-bit range.** The `Integer` twin narrows its argument with
 *    `as i32`; the `Long` helper must not, so `Long.MIN_VALUE`, `Long.MAX_VALUE`
 *    and a value whose low 32 bits are zero (`1L << 32`) are all here. A
 *    narrowing bug reads as `0` on the last one and survives every small-number
 *    test.
 *  * **The unbox.** `longValue()` on a wrapper the fast path allocated, on one
 *    the identity cache returned, and on one built through `new Long(...)`'s
 *    modern replacement (`Long.parseLong` → `valueOf`), plus the null receiver,
 *    which must be an NPE and not a sentinel.
 *  * **A loop.** Every arm runs the same values through a hot loop first, so the
 *    values are read back out of JIT-compiled code and not only out of the
 *    interpreter. A bind that only the interpreter misses would otherwise pass.
 */
public final class LongBoxOracle {

    private static final long[] VALUES = {
        Long.MIN_VALUE, Long.MIN_VALUE + 1, -4294967297L, -4294967296L,
        -1000000L, -129L, -128L, -127L, -1L, 0L, 1L, 126L, 127L, 128L,
        1000000L, 4294967296L, 4294967297L, Long.MAX_VALUE - 1, Long.MAX_VALUE,
    };

    private static long sink;

    public static void main(String[] args) {
        // Warm: drive both helpers through the tiering thresholds so the values
        // below are produced by compiled code, not by the interpreter.
        for (int i = 0; i < 400000; i++) {
            sink += box(i).longValue() + box(-i).longValue();
        }

        System.out.println("== values ==");
        for (long v : VALUES) {
            Long boxed = Long.valueOf(v);
            System.out.println(v + " -> " + boxed.longValue()
                    + " toString=" + boxed
                    + " hash=" + boxed.hashCode()
                    + " equalsSelf=" + boxed.equals(Long.valueOf(v)));
        }

        System.out.println("== identity cache ==");
        for (long v : VALUES) {
            System.out.println(v + " cached=" + (Long.valueOf(v) == Long.valueOf(v)));
        }

        System.out.println("== cache boundary ==");
        for (long v = -130; v <= 130; v++) {
            boolean same = Long.valueOf(v) == Long.valueOf(v);
            boolean expected = v >= -128 && v <= 127;
            if (same != expected) {
                System.out.println("MISMATCH at " + v + " same=" + same);
            }
        }
        System.out.println("boundary scan complete");

        System.out.println("== unbox via widening reads ==");
        Long parsed = Long.valueOf(Long.parseLong("-9223372036854775808"));
        System.out.println("parsed=" + parsed.longValue()
                + " intValue=" + parsed.intValue()
                + " doubleValue=" + parsed.doubleValue());

        System.out.println("== null unbox ==");
        Long nil = null;
        try {
            sink += nil.longValue();
            System.out.println("NO EXCEPTION (wrong)");
        } catch (NullPointerException e) {
            System.out.println("NullPointerException");
        }

        System.out.println("== sum invariant ==");
        long total = 0;
        for (int i = 0; i < 100000; i++) {
            total += Long.valueOf(i * 2654435761L).longValue();
        }
        System.out.println("total=" + total);
        System.out.println("sinkNonZero=" + (sink != 0));
    }

    private static Long box(long v) {
        return Long.valueOf(v);
    }
}
