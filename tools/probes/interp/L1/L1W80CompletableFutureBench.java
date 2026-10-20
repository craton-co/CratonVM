// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 80, lane L1 -- bench for the `JdkCompletableFuture`
// HotSpot-alignment row (`CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE`, default
// OFF). With the row a real-JDK `--compatible` VM runs the JDK's own
// `complete` / `completeValue` / `completeExceptionally` /
// `isCompletedExceptionally` / `thenCombine` / `allOf` / `anyOf` instead of the
// stand-ins; `complete` was kept native as a measured fast path on 2026-09-02
// (`CRATONVM_NATIVE_CF_COMPLETE`, "3.14x slower" without it on a composition
// workload), so this bench re-prices it.
//
// A/B, one binary, interleaved, >= 5 rounds, medians of the per-run minima, JIT
// and `--nojit`:
//   cratonvm --compatible L1W80CompletableFutureBench                                         (row off)
//   CRATONVM_COMPAT_JDK_COMPLETABLE_FUTURE=1 cratonvm --compatible L1W80CompletableFutureBench (row on)
//   cratonvm L1W80CompletableFutureBench                                                      (--jdk-only: the JDK bodies)
//   java L1W80CompletableFutureBench                                                          (HotSpot)
// One row only: `L1W80CompletableFutureBench <row>`.
//
// Rows (stdout, min ns/op over REPS=5 rounds):
//   complete          new CompletableFuture(); complete(i); join();
//   complete-chain    new CF; thenApply(+1); complete(i); join() of the dependent (postComplete fires);
//   complete-exc      new CF; completeExceptionally(ex); isCompletedExceptionally();
//   then-combine      completedFuture(i).thenCombine(completedFuture(1), sum).join();
//   all-of-4          allOf(4 completed futures).join();
//   any-of-4          anyOf(3 incomplete, 1 completed).join();
//   supply-join       supplyAsync(() -> i).join() on the common pool (park / unpark dominated).
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum complete=19999900000 complete-chain=20000100000 complete-exc=200000 then-combine=20000100000 all-of-4=200000 any-of-4=19999900000 supply-join=1999000
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.CompletableFuture;

public class L1W80CompletableFutureBench {
    static final int REPS = 5;
    static final int N = 200_000;
    static final int ASYNC_N = 2_000;
    static final RuntimeException EX = new RuntimeException("bench");

    static long complete() {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            CompletableFuture<Long> f = new CompletableFuture<>();
            f.complete((long) i);
            sum += f.join();
        }
        return sum;
    }

    static long completeChain() {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            CompletableFuture<Long> f = new CompletableFuture<>();
            CompletableFuture<Long> g = f.thenApply(x -> x + 1);
            f.complete((long) i);
            sum += g.join();
        }
        return sum;
    }

    static long completeExc() {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            CompletableFuture<Long> f = new CompletableFuture<>();
            f.completeExceptionally(EX);
            if (f.isCompletedExceptionally()) {
                sum++;
            }
        }
        return sum;
    }

    static long thenCombine() {
        long sum = 0;
        CompletableFuture<Long> one = CompletableFuture.completedFuture(1L);
        for (int i = 0; i < N; i++) {
            sum += CompletableFuture.completedFuture((long) i).thenCombine(one, Long::sum).join();
        }
        return sum;
    }

    static long allOf4() {
        long sum = 0;
        CompletableFuture<?>[] cfs = {
            CompletableFuture.completedFuture(1L), CompletableFuture.completedFuture(2L),
            CompletableFuture.completedFuture(3L), CompletableFuture.completedFuture(4L)};
        for (int i = 0; i < N; i++) {
            CompletableFuture.allOf(cfs).join();
            sum++;
        }
        return sum;
    }

    static long anyOf4() {
        long sum = 0;
        CompletableFuture<Long> p = new CompletableFuture<>();
        CompletableFuture<Long> q = new CompletableFuture<>();
        CompletableFuture<Long> r = new CompletableFuture<>();
        for (int i = 0; i < N; i++) {
            sum += (Long) CompletableFuture.anyOf(p, q, r, CompletableFuture.completedFuture((long) i)).join();
        }
        return sum;
    }

    static long supplyJoin() {
        long sum = 0;
        for (int i = 0; i < ASYNC_N; i++) {
            final long v = i;
            sum += CompletableFuture.supplyAsync(() -> v).join();
        }
        return sum;
    }

    public static void main(String[] args) {
        String only = args.length > 0 ? args[0] : null;
        String[] names = {"complete", "complete-chain", "complete-exc", "then-combine", "all-of-4", "any-of-4",
            "supply-join"};
        long[] per = {N, N, N, N, N, N, ASYNC_N};
        long[] best = new long[names.length];
        java.util.Arrays.fill(best, Long.MAX_VALUE);
        long[] sums = new long[names.length];
        boolean[] ran = new boolean[names.length];
        for (int rep = 0; rep < REPS; rep++) {
            for (int i = 0; i < names.length; i++) {
                if (only != null && !only.equals(names[i])) {
                    continue;
                }
                long t0 = System.nanoTime();
                long s;
                try {
                    switch (i) {
                        case 0: s = complete(); break;
                        case 1: s = completeChain(); break;
                        case 2: s = completeExc(); break;
                        case 3: s = thenCombine(); break;
                        case 4: s = allOf4(); break;
                        case 5: s = anyOf4(); break;
                        default: s = supplyJoin(); break;
                    }
                } catch (Throwable t) {
                    // A stand-in that fails (row off) is reported, not timed.
                    if (rep == 0) {
                        System.out.println(names[i] + " failed: " + t);
                    }
                    continue;
                }
                best[i] = Math.min(best[i], System.nanoTime() - t0);
                sums[i] = s;
                ran[i] = true;
            }
        }
        StringBuilder sb = new StringBuilder("checksum");
        for (int i = 0; i < names.length; i++) {
            if (!ran[i]) {
                continue;
            }
            System.out.printf("%-15s %.1f ns/op%n", names[i], best[i] / (double) per[i]);
            sb.append(' ').append(names[i]).append('=').append(sums[i]);
        }
        System.out.println(sb);
        System.out.println("done");
    }
}
