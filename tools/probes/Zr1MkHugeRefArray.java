// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zgc round zr, wave z1, lane mk (2026-10-12).
//
// The serial stop-the-world marker on huge reference arrays.
//
// Before zr z1/mk the serial loop pushed EVERY element of a reference array
// onto its work stack in one go and decided at pop time, so a live
// Object[N] grew an N-entry Vec<usize> (8N bytes, by doubling) inside the
// pause, whether its elements were fresh or long marked. With
// CRATONVM_ZGC_MARK_CLAIM_ON_PUSH (default on) the marker claims on push and
// scans arrays longer than CRATONVM_ZGC_MARK_REF_CHUNK (4096) in
// continuations, so the stack is bounded by the chunk.
//
// Three shapes, each held live across `rounds` System.gc() calls:
//   fresh  - Object[n] of n distinct small objects (every edge is a new mark)
//   shared - Object[n] whose elements all name ONE object (every edge but one
//            is already marked: pure queue churn on the old loop)
//   nested - Object[k][n/k] (arrays of arrays: continuation nesting)
//
// Read on CratonVM with --verbose:gc:
//   [GC] zgc-mark-serial: ... chunks=<c> stack_peak=<p> claim_on_push=1
//   [GC] zgc-pause: ... mark_us=<us>
// Output (HotSpot prints the same):
//   ZR1MK-HUGE n=<n> rounds=<r> fresh=<n> shared=<n> nested=<n> ok
public class Zr1MkHugeRefArray {
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        Object[] fresh = new Object[n];
        for (int i = 0; i < n; i++) fresh[i] = new int[] {i};
        Object one = new int[] {42};
        Object[] shared = new Object[n];
        for (int i = 0; i < n; i++) shared[i] = one;
        int k = 64;
        Object[][] nested = new Object[k][];
        for (int j = 0; j < k; j++) {
            nested[j] = new Object[n / k];
            for (int i = 0; i < n / k; i++) nested[j][i] = new int[] {j, i};
        }
        long t0 = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            System.gc();
        }
        long gcMs = (System.nanoTime() - t0) / 1_000_000;
        long f = 0;
        for (int i = 0; i < n; i++) f += ((int[]) fresh[i])[0] == i ? 1 : 0;
        long s = 0;
        for (int i = 0; i < n; i++) s += shared[i] == one ? 1 : 0;
        long q = 0;
        for (int j = 0; j < k; j++)
            for (int i = 0; i < n / k; i++) {
                int[] e = (int[]) nested[j][i];
                q += (e[0] == j && e[1] == i) ? 1 : 0;
            }
        boolean ok = f == n && s == n && q == (long) k * (n / k);
        System.out.println("ZR1MK-HUGE n=" + n + " rounds=" + rounds + " fresh=" + f + " shared=" + s
                + " nested=" + q + (ok ? " ok" : " BAD"));
        System.err.println("ZR1MK-HUGE gc_wall_ms=" + gcMs);
    }
}
