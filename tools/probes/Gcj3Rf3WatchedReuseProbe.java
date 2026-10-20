// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gcj j3/rf3: the measurement and soundness probe of
// CRATONVM_GEN_WATCHED_SET_REUSE_UNCHANGED (opt-in;
// docs/internal/proposals/gc/gcj-j1rf-proposal-watched-set-maintained-by-the-processor-20261019.md).
//
// Shape: <refs> WeakReferences to strongly held objects, both tenured by a few
// System.gc() calls, so the reference processor's watched list is the same
// from one young pause to the next; then <rounds> rounds of young garbage
// churn (the young pauses that list is published for), with a few interned
// Strings per round (the String table's adds into the published row) and one
// check per round that a sample of the references still answers its referent.
// At the end the strong holders are dropped and the references must clear.
//
// stdout (deterministic; HotSpot 25 -XX:+UseSerialGC), for `<refs> <rounds>`:
//   refs=<refs> rounds=<rounds>
//   intact=<rounds>/<rounds>
//   cleared=<refs>/<refs>
//   PROBE-OK
// stderr: `elapsed-ms=`. The process CPU time for the A/B is the battery's `<row>.hb` last
// `cpu_ms` (battery-observe.ps1).
import java.lang.ref.WeakReference;

public class Gcj3Rf3WatchedReuseProbe {
    static Object[] strong;
    static volatile Object sink;

    // The referents are created and checked in their own frames, so no local
    // of main() can keep one alive past the final collections.
    static WeakReference<?>[] build(int refs) {
        strong = new Object[refs];
        WeakReference<?>[] weak = new WeakReference<?>[refs];
        for (int i = 0; i < refs; i++) {
            Object o = new int[] {i};
            strong[i] = o;
            weak[i] = new WeakReference<>(o);
        }
        return weak;
    }

    static boolean round(int r, WeakReference<?>[] weak) {
        // ~4 MB of young garbage per round: several young pauses per few rounds.
        for (int k = 0; k < 4096; k++) {
            sink = new byte[1024];
        }
        sink = ("gcj3rf3-" + (r % 64)).intern();
        boolean ok = true;
        for (int s = 0; s < 64; s++) {
            int i = (int) (((long) s * 7919L + r) % weak.length);
            Object got = weak[i].get();
            if (got != strong[i] || ((int[]) got)[0] != i) {
                ok = false;
            }
        }
        return ok;
    }

    static int countCleared(WeakReference<?>[] weak) {
        int cleared = 0;
        for (WeakReference<?> w : weak) {
            if (w.get() == null) {
                cleared++;
            }
        }
        return cleared;
    }

    public static void main(String[] args) throws Exception {
        int refs = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        long t0 = System.nanoTime();
        WeakReference<?>[] weak = build(refs);
        for (int i = 0; i < 4; i++) {
            System.gc();
        }
        int intact = 0;
        for (int r = 0; r < rounds; r++) {
            if (round(r, weak)) {
                intact++;
            }
        }
        strong = null;
        sink = null;
        for (int i = 0; i < 4; i++) {
            System.gc();
        }
        int cleared = countCleared(weak);
        System.out.println("refs=" + refs + " rounds=" + rounds);
        System.out.println("intact=" + intact + "/" + rounds);
        System.out.println("cleared=" + cleared + "/" + refs);
        System.out.println(intact == rounds && cleared == refs ? "PROBE-OK" : "PROBE-FAIL");
        System.err.println("elapsed-ms=" + (System.nanoTime() - t0) / 1_000_000L);
    }
}
