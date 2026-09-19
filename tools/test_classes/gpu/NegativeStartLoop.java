public class NegativeStartLoop {
    // Negative compile-time-constant loop start: `for (i = -2; i < n;
    // i++)`. `-2` is outside the `iconst_m1..iconst_5` range (which
    // only covers -1), so javac emits `bipush -2` — already handled by
    // `resolve_start_value`'s existing sign-extending `bipush` decode,
    // which correctly recovers the value -2.
    //
    // Unlike a non-negative start (see `EligibleOffsetLoop.java`), a
    // negative start is REJECTED by `loop_recog::resolve_start_value`
    // — not because `tid + K` would compute the wrong register value,
    // but because of how the VM sizes the CUDA grid: the host launches
    // `>= bound` threads (sized off the array length), but a negative
    // `K` needs `bound - K > bound` threads to cover the loop's full
    // iteration range. A `bound`-sized launch would silently
    // under-provision threads and drop the tail of the loop. See
    // `jit-cuda/src/lowering/loop_recog.rs`'s module doc comment for
    // the full derivation.
    //
    // (This method is never actually executed against a real array in
    // these unit tests — like every other non-canonical-shape fixture
    // in this directory, only its bytecode SHAPE is inspected. Were it
    // executed, `a[-2]` would throw `ArrayIndexOutOfBoundsException`
    // on the very first iteration, same as it would on the CPU
    // interpreter this loop correctly falls back to.)
    public static void negativeStart(int[] a) {
        int n = a.length;
        for (int i = -2; i < n; i++) {
            a[i] = 0;
        }
    }
}
