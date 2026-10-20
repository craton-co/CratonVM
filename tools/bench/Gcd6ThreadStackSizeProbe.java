// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcd d6/f (2026-09-28): {@code new Thread(group, target, name, stackSize)}
 * must get a stack of that size
 * ({@code docs/internal/gc/gcd-d6s-thread-stack-size-argument-is-ignored-FIXED-20260929.md}).
 *
 * <p>A thread constructed with a 256 MiB {@code stackSize} recurses 200 000
 * levels through one static method ({@code down}, not tail-recursive, so no
 * tier can flatten it); a thread constructed without one runs the same
 * recursion and overflows, as on HotSpot (its default stack is 1 MiB; 200 000
 * frames need several MiB on any tier). Both threads catch their own
 * {@code StackOverflowError}, so the run ends on its own either way.
 *
 * <p>Deterministic under HotSpot (checked on Windows and WSL Linux, JDK 25):
 * <pre>
 *   deep thread (stackSize 256 MiB): depth 200000 sum 20000100000
 *   default thread: StackOverflowError
 *   PASS
 * </pre>
 * Commands:
 * <pre>
 *   javac -d /tmp/gcd6stack tools/bench/Gcd6ThreadStackSizeProbe.java
 *   java -XX:+UseSerialGC -cp /tmp/gcd6stack Gcd6ThreadStackSizeProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -cp /tmp/gcd6stack Gcd6ThreadStackSizeProbe
 * </pre>
 * On CratonVM the deep line needs BOTH halves of the fix: the 264 MiB carrier
 * (gcd d6/f, {@code vm_exec::carrier_stack_bytes}) and a compiled
 * self-recursion budget that follows the thread's stack (JIT round 13,
 * {@code SELF_CALL_STACK_BUDGET}; the exact edit is on the page). With only
 * the first, expect {@code deep thread (stackSize 256 MiB): StackOverflowError}
 * and {@code FAIL deep}: the fixed 4 MiB budget trips first.
 */
public class Gcd6ThreadStackSizeProbe {
    static final int DEPTH = 200_000;

    static long down(int n) {
        return n == 0 ? 0 : n + down(n - 1);
    }

    /** Runs the recursion on a thread with {@code stackSize} (0 = none). */
    static String run(String name, long stackSize) throws InterruptedException {
        final String[] result = new String[1];
        Runnable body = () -> {
            try {
                long sum = down(DEPTH);
                result[0] = "depth " + DEPTH + " sum " + sum;
            } catch (StackOverflowError e) {
                result[0] = "StackOverflowError";
            }
        };
        Thread t = stackSize > 0
                ? new Thread(null, body, name, stackSize)
                : new Thread(null, body, name);
        t.start();
        t.join();
        return result[0];
    }

    public static void main(String[] args) throws InterruptedException {
        // Warm `down` up on a shallow depth so it is compiled before the deep run.
        long warm = 0;
        for (int i = 0; i < 2_000; i++) {
            warm += down(100);
        }
        if (warm != 2_000L * 5_050L) {
            System.out.println("FAIL warm-up sum " + warm);
            return;
        }
        String deep = run("deep", 256L << 20);
        System.out.println("deep thread (stackSize 256 MiB): " + deep);
        String plain = run("plain", 0);
        System.out.println("default thread: " + plain);
        String expectedDeep = "depth " + DEPTH + " sum " + ((long) DEPTH * (DEPTH + 1) / 2);
        if (!expectedDeep.equals(deep)) {
            System.out.println("FAIL deep");
        } else if (!"StackOverflowError".equals(plain)) {
            System.out.println("FAIL default");
        } else {
            System.out.println("PASS");
        }
    }
}
