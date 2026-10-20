/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

import java.util.Arrays;

/**
 * gci i10/cr (2026-10-13): the per-call cost of an INSTANCE JNI native called
 * virtually ({@code invokevirtual}) -- the virtual half of
 * {@code gci-i8dm-jni-native-call-sites-re-resolve-by-name-every-call-20261013}.
 * Under {@code --nojit} every call takes the interpreter's virtual fallback,
 * which dispatches on the receiver's class with a JVMS 5.4.6 selection; the
 * i10/cr door memo serves those calls ({@code CRATONVM_JNI_DISPATCH_MEMO_SELECTED}).
 *
 * <p>Reuses the i8/dm native library: {@code GciDm8JniMemoProbe.Base.instAdd}
 * ({@code x + 10}), called on a {@code GciDm8JniMemoProbe.Sub} receiver (the
 * inherited native). Needs {@code GciDm8JniMemoProbe} on the class path.
 *
 * <p>stdout is deterministic ({@code inst: ok}, HotSpot prints it too); the
 * number is on stderr as {@code [gci-cr10-jni-virtual] inst median=<n>ns/call
 * reps=[...]}. Usage: {@code GciCr10JniVirtualCostProbe /abs/path/gcidm8jnimemo.dll [calls]}.
 */
public final class GciCr10JniVirtualCostProbe {
    static final int REPS = 7;
    static final int CALLS = 200_000;

    static long run(GciDm8JniMemoProbe.Base b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += b.instAdd(i);
        }
        return s;
    }

    public static void main(String[] args) {
        if (args.length < 1) {
            System.out.println("usage: GciCr10JniVirtualCostProbe /abs/path/gcidm8jnimemo.dll [calls]");
            return;
        }
        System.load(args[0]);
        final int calls = args.length > 1 ? Integer.parseInt(args[1]) : CALLS;
        GciDm8JniMemoProbe.Base b = new GciDm8JniMemoProbe.Sub();
        final long expected = (long) calls * (calls - 1) / 2 + 10L * calls;
        run(b, calls / 10); // warm-up; checksum unchecked
        long[] ns = new long[REPS];
        boolean ok = true;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            long s = run(b, calls);
            ns[r] = (System.nanoTime() - t0) / calls;
            ok &= s == expected;
        }
        long[] sorted = ns.clone();
        Arrays.sort(sorted);
        System.err.println("[gci-cr10-jni-virtual] inst median=" + sorted[REPS / 2]
                + "ns/call reps=" + Arrays.toString(ns));
        System.out.println(ok ? "inst: ok" : "inst: FAIL checksum");
    }
}
