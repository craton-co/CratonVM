// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 38, lane L7: a thread's requested stack size
// governs how deep it may recurse, as on HotSpot. `new Thread(group, task,
// name, stackSize)` and `Thread.ofPlatform().stackSize(n)` used to be ignored:
// every thread stopped at the interpreter's 8192-frame limit
// (`VmConfig::max_stack_depth`), so `--nojit` threw `StackOverflowError` at
// depth 8192 in a thread that asked for 256 MiB (wave 37's
// `L7W37ArgOverlapProbe` last row printed `thread 18003000 0.0`). The
// requested size now raises that thread's limit (8192 frames per MiB,
// `FrameStack::at_frame_limit`) and its carrier's native stack; `-Xss` raises
// the default of every thread.
//
// Rows:
//   main            the main thread at 3000 (control: unchanged);
//   requested-256m  the wave-37 row, 9000 levels of a double recursion;
//   builder-32m     the same through Thread.Builder.OfPlatform.stackSize;
//   requested-64m   30000 levels (the interpreter allows 524288 here; with the
//                   JIT on, compiled self-recursion has its own fixed 4 MiB
//                   budget, see
// docs/internal/fixed-bugs/interpreter-L7-compiled-recursion-ignores-a-threads-requested-stack-size-FIXED-20261005.md);
//   requested-runaway / default-runaway   unbounded recursion still ends in a
//                   catchable StackOverflowError with and without a request.
//
// Run: cratonvm -cp <dir> L7W38ThreadStackSize   (default, --nojit, --compatible)
// Positive control: CRATONVM_DBG_THREADSTART=1 prints, for the "deep" thread,
//   `[THREADSTART] tid=N name="deep" run-class=java/lang/Thread
//   native-stack=276824064 frame-limit=2097152` on stderr (8388608 / 8192 for
//   "plain").
//
// HotSpot 25 (25.0.3) stdout, identical with -Xint:
//   main 3000.0
//   requested-256m 9000.0
//   builder-32m 40504500
//   requested-64m 450015000
//   requested-runaway soe
//   default-runaway soe
public class L7W38ThreadStackSize {

    static double deepDouble(double acc, int n) {
        return n == 0 ? acc : deepDouble(acc + 1.0, n - 1);
    }

    static long sumTo(int n) {
        return n == 0 ? 0 : n + sumTo(n - 1);
    }

    static int runaway(int n) {
        return runaway(n + 1) + 1;
    }

    static String runawayCaught() {
        try {
            runaway(0);
            return "returned";
        } catch (StackOverflowError e) {
            return "soe";
        }
    }

    static String inThread(String name, long stackSize, java.util.function.Supplier<String> work)
            throws InterruptedException {
        final String[] out = new String[1];
        Thread t = new Thread(null, () -> out[0] = work.get(), name, stackSize);
        t.start();
        t.join();
        return out[0];
    }

    public static void main(String[] args) throws Exception {
        System.out.println("main " + deepDouble(0.0, 3000));

        System.out.println("requested-256m "
                + inThread("deep", 256L << 20, () -> String.valueOf(deepDouble(0.0, 9000))));

        final String[] b = new String[1];
        Thread bt = Thread.ofPlatform().name("builder").stackSize(32L << 20)
                .unstarted(() -> b[0] = String.valueOf(sumTo(9000)));
        bt.start();
        bt.join();
        System.out.println("builder-32m " + b[0]);

        System.out.println("requested-64m "
                + inThread("deep64", 64L << 20, () -> String.valueOf(sumTo(30000))));

        System.out.println("requested-runaway "
                + inThread("runaway16", 16L << 20, L7W38ThreadStackSize::runawayCaught));

        System.out.println("default-runaway "
                + inThread("plain", 0L, L7W38ThreadStackSize::runawayCaught));
    }
}
