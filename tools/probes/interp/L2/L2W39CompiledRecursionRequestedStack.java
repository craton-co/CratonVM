// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 39, lane L2:
// `docs/internal/fixed-bugs/interpreter-L7-compiled-recursion-ignores-a-threads-requested-stack-size-FIXED-20261005.md`.
// Wave 38 made a thread's requested stack size raise the interpreter's frame
// limit (8192 frames per MiB); compiled self-recursion kept its fixed 4 MiB
// budget (`SELF_CALL_STACK_BUDGET`, ~10k-21k compiled frames), so a thread
// that asked for 32 MiB threw `StackOverflowError` at a depth `--nojit`
// completes. The budget now scales with the thread's carrier
// (`jit::helpers::self_call_stack_budget`).
//
// Each recursion runs on its own thread after the method was warmed (called
// 20 000 times at depth 50 on that thread, so it is compiled before the deep
// call). Rows:
//   requested-32m-60000   `new Thread(null, r, "deep", 32 MiB)`, 60 000 levels
//   builder-32m-60000     the same through Thread.ofPlatform().stackSize
//   default-runaway       unbounded recursion on a default thread: still soe
//   requested-runaway     unbounded recursion on a 32 MiB thread: still soe
//
// Run: javac -d out L2W39CompiledRecursionRequestedStack.java
//      cratonvm [--nojit] -cp out L2W39CompiledRecursionRequestedStack
//
// Expected HotSpot 25 output (default and -Xint), and CratonVM's in every
// mode:
//   requested-32m-60000 1800030000
//   builder-32m-60000 1800030000
//   default-runaway soe
//   requested-runaway soe
//
// Before wave 39 (default mode, JIT on) the two 60000 rows were expected to
// print `soe`: the compiled frames stop at ~10k-21k.
//
// Positive control (CratonVM, CRATONVM_DBG_JITC=1, stderr): for each 32 MiB
// thread, one line
//   [cratonvm-jitc] self-call stack budget scaled: carrier=41943040 budget=37748736
// and no `self-call stack guard TRIP` line for the 60000 rows.
public class L2W39CompiledRecursionRequestedStack {

    static long sumTo(int n) {
        return n == 0 ? 0 : n + sumTo(n - 1);
    }

    static int runaway(int n) {
        return runaway(n + 1) + 1;
    }

    static String deep(int levels) {
        long warm = 0;
        for (int i = 0; i < 20_000; i++) {
            warm += sumTo(50);
        }
        if (warm != 20_000L * 1275) {
            return "bad-warmup";
        }
        try {
            return Long.toString(sumTo(levels));
        } catch (StackOverflowError e) {
            return "soe";
        }
    }

    static String runawayCaught() {
        try {
            runaway(0);
            return "returned";
        } catch (StackOverflowError e) {
            return "soe";
        }
    }

    static String onThread(Thread.Builder.OfPlatform builder, long stackSize,
            java.util.function.Supplier<String> task) throws Exception {
        String[] out = new String[1];
        Runnable r = () -> out[0] = task.get();
        Thread t = builder == null
                ? new Thread(null, r, "deep", stackSize)
                : builder.stackSize(stackSize).unstarted(r);
        t.start();
        t.join();
        return out[0];
    }

    public static void main(String[] args) throws Exception {
        long m32 = 32L << 20;
        System.out.println("requested-32m-60000 " + onThread(null, m32, () -> deep(60_000)));
        System.out.println("builder-32m-60000 "
                + onThread(Thread.ofPlatform().name("builder"), m32, () -> deep(60_000)));
        System.out.println("default-runaway " + onThread(null, 0, L2W39CompiledRecursionRequestedStack::runawayCaught));
        System.out.println("requested-runaway " + onThread(null, m32, L2W39CompiledRecursionRequestedStack::runawayCaught));
    }
}
