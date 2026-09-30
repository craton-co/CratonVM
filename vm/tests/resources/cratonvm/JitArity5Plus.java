// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

/**
 * Synthetic fixture for the JIT arity-5+ bailout regression
 * (vm/tests/jit_arity_5plus.rs, review §2.3 item 4).
 *
 * <p>Per `vm/src/jit/helpers.rs:243-248` the JIT's
 * `call_jit_compiled_method_entry` ABI table covers 0..=4 args
 * (no-ctx) and 0..=3 args (with-ctx); 5+-arg callees take the
 * `bail_to_interpreter` slow path. This fixture exercises that
 * bailout end-to-end via 0-arg entry points that the Rust test
 * invokes through `Vm::invoke`. The 6-arg call lives entirely
 * within Java bytecode so it travels through the regular
 * `invokestatic` dispatch path (and, if the JIT warmed the
 * caller, through `jit_invoke_dispatch` → bailout).
 *
 * <ul>
 *   <li>{@link #sum6(int, int, int, int, int, int)} — 6 int args,
 *       returns long. Above the register-arg ceiling on both ABI
 *       variants, so any JIT call site that admitted the callee
 *       (e.g. via warm-up from {@link #drive100()}) is forced to
 *       bail.</li>
 *   <li>{@link #callSmall()} — entry point: calls sum6 with
 *       literals 1..6, returns the long aggregate (= 21).</li>
 *   <li>{@link #callLarge()} — entry point: calls sum6 with
 *       literals 100..600, returns 2100L. Larger inputs detect
 *       any narrowing / arg-drop in the bailout's value-stack
 *       reconstruction (a missing arg shifts the sum by &gt;= 100).</li>
 *   <li>{@link #drive100()} — invokes sum6 100 times with
 *       arithmetically-derived inputs so the JIT profiler can
 *       observe a hot call site, then returns the loop sum.</li>
 * </ul>
 *
 * Expected (computed by Rust oracles in jit_arity_5plus.rs):
 * <ul>
 *   <li>{@code callSmall() == 21L}</li>
 *   <li>{@code callLarge() == 2100L}</li>
 *   <li>{@code drive100() ==} <code>sum_{i=0..99}(6*i + 15)</code> = 31_200L</li>
 * </ul>
 */
public class JitArity5Plus {

    /**
     * 6-argument static method. Forces the JIT call path above its
     * 4-register-arg ABI ceiling. Returning {@code long} guarantees
     * the result occupies a 2-slot stack position (independent
     * coverage for the value-stack reconstruction inside the
     * interpreter bail).
     */
    public static long sum6(int a, int b, int c, int d, int e, int f) {
        // Promote to long *before* summing so the result cannot be
        // confused with a truncated-to-int aggregate.
        return (long) a + (long) b + (long) c + (long) d + (long) e + (long) f;
    }

    /** 0-arg entry — calls sum6(1..6) once and returns 21L. */
    public static long callSmall() {
        return sum6(1, 2, 3, 4, 5, 6);
    }

    /** 0-arg entry — calls sum6(100..600) once and returns 2100L. */
    public static long callLarge() {
        return sum6(100, 200, 300, 400, 500, 600);
    }

    /**
     * 0-arg entry — loops 100 times calling sum6 with
     * arithmetically-varying inputs. Provides a hot call site for
     * the JIT profiler so the bailout fast path actually triggers
     * on later loop iterations.
     *
     * <p>Closed form: total = sum_{i=0..99}(6*i + 15) = 6*4950 + 1500 = 31_200.
     */
    public static long drive100() {
        long total = 0L;
        for (int i = 0; i < 100; i++) {
            total += sum6(i, i + 1, i + 2, i + 3, i + 4, i + 5);
        }
        return total;
    }
}
