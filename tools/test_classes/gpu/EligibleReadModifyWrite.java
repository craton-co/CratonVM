// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * An array that is both read and written in the loop body.
 *
 * This is the shape a chunked writeback must NOT stream out early: it
 * commits each chunk into the Java array as that chunk's event fires,
 * which is before the bounds-failure flag has been read. For a
 * write-only array that is harmless (a CPU re-run rewrites the same
 * elements), but here the partial commit would become the re-run's own
 * INPUT and change its result. `reads_param_mask` is what lets the
 * dispatch tell the two apart.
 */
public class EligibleReadModifyWrite {
    public static void bump(int[] a) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            a[i] = a[i] + 1;
        }
    }
}
