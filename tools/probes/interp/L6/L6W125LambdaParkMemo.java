// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 125, lane L6 (page
// docs/known-issues/interpreter/i125-L6-virtual-thread-owned-frames-miss-lambda-memo-and-osr-20261226.md).
//
// A method reference `Supplier<Integer> sup = L6W125LambdaParkMemo::parkLeaf`
// is called ONLY on a virtual thread, and its impl parks on every call before
// a loop. The lambda door's static arm memoises the impl owner
// (`record_lambda_global_impl_owner`) only after `invoke_static_shared(..)?`
// RETURNS; every call here yields through that `?` first, so by default the
// memo is never filled. Every round then runs `parkLeaf` through by-name
// `interpreter::execute`, which pushes an OWNED frame, and the virtual
// thread's OSR offer (`try_osr_offer`) refuses a frame without
// `cached_method()`. The loop stays interpreted on every round.
//
//   gap (a): CRATONVM_LAMBDA_OWNER_MEMO_ON_YIELD=1 records the owner on the
//            yield (class initialized): round 1 on takes the memoised arm
//            (`try_invoke_cached_lambda_impl`, a CACHED frame), whose loop
//            may be offered OSR after the remount.
//   gap (b): CRATONVM_VTHREAD_EXECUTE_CACHED_FRAME=1 makes `execute` push a
//            cached frame on a virtual thread, so even the by-name arm's
//            frame may be offered OSR.
//
// The TIME rows time ONLY the loop after the park (inside `parkLeaf`), so the
// park's timer granularity does not enter them.
//
// Run: javac -d . L6W125LambdaParkMemo.java
//      java -cp . L6W125LambdaParkMemo                               (HotSpot)
//   OFF (defaults):  CRATONVM_DBG=jit-compiled cratonvm --jdk-only -cp . L6W125LambdaParkMemo
//   A:  CRATONVM_LAMBDA_OWNER_MEMO_ON_YIELD=1 CRATONVM_DBG=jit-compiled cratonvm --jdk-only -cp . L6W125LambdaParkMemo
//   B:  CRATONVM_VTHREAD_EXECUTE_CACHED_FRAME=1 CRATONVM_DBG=jit-compiled cratonvm --jdk-only -cp . L6W125LambdaParkMemo
//   each also with --compatible, and with --nojit (stdout only).
//   Pass: stdout (TIME lines excluded) = the block below in every arm.
//   Gain: an `ir-osr` line naming `parkLeaf()I` in A and B and none in OFF;
//   `TIME loop-median-late-us` in A (and B) well below OFF.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags and -Xint)
// rounds 40 boxed 40
// first 99121004
// sum -330127136
// HOTSPOT_EXPECTED_END

import java.util.Arrays;
import java.util.concurrent.locks.LockSupport;
import java.util.function.Supplier;

public class L6W125LambdaParkMemo {
    static final int ROUNDS = 40;
    static final int ITER = 400_000;
    static final long[] loopNanos = new long[ROUNDS];
    static int round;

    /** Parks (an unmount under the lambda door), then a loop. */
    static int parkLeaf() {
        LockSupport.parkNanos(20_000L);
        long t0 = System.nanoTime();
        int s = round;
        for (int i = 0; i < ITER; i++) {
            s += (i * 31) ^ (s >>> 3);
        }
        loopNanos[round] = System.nanoTime() - t0;
        return s;
    }

    public static void main(String[] args) throws Throwable {
        Supplier<Integer> sup = L6W125LambdaParkMemo::parkLeaf;
        StringBuilder out = new StringBuilder();
        Thread t = Thread.ofVirtual().start(() -> {
            int boxed = 0;
            int sum = 0;
            Object first = null;
            for (round = 0; round < ROUNDS; round++) {
                Object v = sup.get();
                if (round == 0) {
                    first = v;
                }
                if (v instanceof Integer) {
                    boxed++;
                    sum += (Integer) v;
                }
            }
            out.append("rounds ").append(ROUNDS).append(" boxed ").append(boxed).append('\n');
            out.append("first ").append(first).append('\n');
            out.append("sum ").append(sum).append('\n');
        });
        t.join();
        System.out.print(out);
        long[] late = Arrays.copyOfRange(loopNanos, ROUNDS / 2, ROUNDS);
        Arrays.sort(late);
        long total = 0;
        for (int r = 1; r < ROUNDS; r++) {
            total += loopNanos[r];
        }
        System.out.println("TIME loop-median-late-us " + late[late.length / 2] / 1_000);
        System.out.println("TIME loop-total-rounds-1-on-ms " + total / 1_000_000);
    }
}
