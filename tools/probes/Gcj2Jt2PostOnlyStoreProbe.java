// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcj j2/jt2 (2026-10-19): old-holder / young-value reference stores in tight,
 * call-free compiled loops on several threads while young collections run, so
 * the BUG-03 take-over freezes peers at arbitrary instructions of the inline
 * card barrier. The soundness row of {@code CRATONVM_JIT_POST_ONLY_CARD_MARK}
 * (the barrier has no PRE card check; the take-over completes the card of a
 * peer frozen between a store and its card byte). Proposal:
 * {@code docs/internal/proposals/gc/gcj-j1jt-proposal-takeover-completes-the-card-mark-20261019.md}.
 *
 * <p>Each worker owns a tenured {@code Leaf[]} table and tenured {@code Holder}s.
 * Per round it allocates {@code n} young {@code Leaf}s, stores them into the
 * table ({@code aastore}) and into the holders' fields ({@code putfield}) in
 * call-free loops, repeated {@code reps} times (the steady state: the card is
 * already dirty), drops its own young array, churns young garbage (so every
 * worker's stores meet young collections started by the others), and then
 * checks every table element and holder field: the right {@code Leaf}, with
 * its value and its check word intact. A card lost by a frozen peer frees its
 * young {@code Leaf} at the next young collection; the check then reads a
 * reused or cleared object.
 *
 * <p>Args: {@code [threads rounds n reps mode]} (defaults 4 60 4096 8 run).
 * {@code mode=corrupt} is the checker's positive control: worker 0 nulls one
 * table element after its stores every round, so {@code bad} must be
 * {@code rounds} there (the stdout line then DIFFERS from HotSpot's run of the
 * default mode, and matches HotSpot's run of the same mode).
 *
 * <p>Stdout (HotSpot {@code -XX:+UseSerialGC}, any collector):
 * {@code gcj2jt2 post-only-store threads=T rounds=R n=N reps=P mode=M bad=B}.
 * Timing on stderr.
 */
public final class Gcj2Jt2PostOnlyStoreProbe {
    static final class Leaf {
        final long v;
        final long check;

        Leaf(long v) {
            this.v = v;
            this.check = ~v;
        }
    }

    static final class Holder {
        Leaf a;
        Leaf b;
        long pad;
    }

    static volatile Object sink;

    /** The timed table store: {@code table} is old, {@code src[i]} young. */
    static void storeTable(Leaf[] table, Leaf[] src) {
        for (int i = 0; i < src.length; i++) {
            table[i] = src[i];
        }
    }

    /** The timed field stores: every holder is old, every value young. */
    static void storeFields(Holder[] holders, Leaf[] src) {
        int m = holders.length;
        for (int i = 0; i < m; i++) {
            Holder h = holders[i];
            h.a = src[i];
            h.b = src[src.length - 1 - i];
        }
    }

    static int check(Leaf[] table, Holder[] holders, long base) {
        int bad = 0;
        int n = table.length;
        for (int i = 0; i < n; i++) {
            Leaf l = table[i];
            if (l == null || l.v != base + i || l.check != ~(base + i)) {
                bad++;
            }
        }
        for (int i = 0; i < holders.length; i++) {
            Holder h = holders[i];
            Leaf a = h.a;
            Leaf b = h.b;
            if (a == null || a.v != base + i || a.check != ~(base + i)) {
                bad++;
            }
            long bj = base + (n - 1 - i);
            if (b == null || b.v != bj || b.check != ~bj) {
                bad++;
            }
        }
        return bad;
    }

    static void churn(int kb) {
        Object keep = null;
        for (int i = 0; i < kb; i++) {
            byte[] chunk = new byte[1024];
            chunk[i & 1023] = (byte) i;
            if ((i & 63) == 0) {
                keep = chunk;
            }
        }
        sink = keep;
    }

    public static void main(String[] args) throws Exception {
        final int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        final int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 60;
        final int n = args.length > 2 ? Integer.parseInt(args[2]) : 4096;
        final int reps = args.length > 3 ? Integer.parseInt(args[3]) : 8;
        final String mode = args.length > 4 ? args[4] : "run";
        final boolean corrupt = mode.equals("corrupt");

        final Leaf[][] tables = new Leaf[threads][];
        final Holder[][] holders = new Holder[threads][];
        for (int t = 0; t < threads; t++) {
            tables[t] = new Leaf[n];
            holders[t] = new Holder[n / 4];
            for (int i = 0; i < holders[t].length; i++) {
                holders[t][i] = new Holder();
            }
        }
        // Tenure the tables and the holders (a System.gc() promotes every
        // survivor on Serial and on CratonVM's Generational backend).
        System.gc();
        System.gc();

        final int[] bad = new int[threads];
        final Thread[] workers = new Thread[threads];
        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            final int id = t;
            workers[t] = new Thread(() -> {
                Leaf[] table = tables[id];
                Holder[] hs = holders[id];
                for (int r = 0; r < rounds; r++) {
                    long base = ((long) id << 40) | ((long) r << 20);
                    Leaf[] src = new Leaf[n];
                    for (int i = 0; i < n; i++) {
                        src[i] = new Leaf(base + i);
                    }
                    for (int p = 0; p < reps; p++) {
                        storeTable(table, src);
                        storeFields(hs, src);
                    }
                    if (corrupt && id == 0) {
                        table[r % n] = null;
                    }
                    src = null;
                    churn(2048);
                    bad[id] += check(table, hs, base);
                }
            }, "gcj2jt2-store-" + t);
            workers[t].start();
        }
        for (Thread w : workers) {
            w.join();
        }
        long total = 0;
        for (int b : bad) {
            total += b;
        }
        System.err.println("[bench-timing] gcj2jt2 post-only-store wall_ms="
                + (System.nanoTime() - t0) / 1_000_000);
        System.out.println("gcj2jt2 post-only-store threads=" + threads + " rounds=" + rounds
                + " n=" + n + " reps=" + reps + " mode=" + mode + " bad=" + total);
    }
}
