// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * A bounds failure on a kernel big enough that the CHUNKED writeback is
 * active — the one path that can leave part of an output array written
 * when the deopt happens.
 *
 * `BoundsDeopt2` does not reach this path: its output is half the length
 * of the arrays the loop iterates over, so the chunk planner refuses it.
 * Here the loop bound IS the output's length, the output is write-only,
 * and it is 2^21 elements, so chunking engages and several chunks commit
 * into the Java array before the failing chunk is known.
 *
 * `in` is half the length of `out`, so `in[i]` runs off the end exactly
 * half way. Plain Java writes `out[0 .. in.length)` and then throws, and
 * that is precisely what CratonVM must still produce: the interpreter
 * re-runs the whole method after the deopt and rewrites every element it
 * would have written, so any chunk the GPU committed early is overwritten
 * with the same value. Compare this program's output under `--gpu`
 * against HotSpot; they must agree exactly, including the element counts.
 */
public class BoundsDeoptChunked {
    public static void main(String[] args) {
        int outLen = 1 << 21;
        int inLen = outLen / 2;
        int[] in = new int[inLen];
        int[] out = new int[outLen];
        for (int i = 0; i < inLen; i++) {
            in[i] = i;
        }
        String thrown = "NONE";
        try {
            EligibleInlineLengthBound.scaleInline(in, out);
        } catch (Throwable e) {
            thrown = e.getClass().getName();
        }
        // How much of `out` ended up written, and does it hold the right
        // values? Java's answer is "exactly the first inLen elements, each
        // in[i]*3"; anything else means the deopt left observable partial
        // GPU state.
        int written = 0;
        int wrong = 0;
        for (int i = 0; i < outLen; i++) {
            if (out[i] != 0) {
                written++;
            }
            int want = i < inLen ? in[i] * 3 : 0;
            if (out[i] != want) {
                wrong++;
            }
        }
        System.out.println("CHUNKEDDEOPT thrown=" + thrown
                + " nonzero=" + written
                + " mismatched=" + wrong
                + " out0=" + out[0]
                + " outMid=" + out[inLen - 1]
                + " outAfter=" + out[inLen]
                + " outLast=" + out[outLen - 1]);
    }
}
