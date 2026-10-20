package cratonvm;

/**
 * Session 35: JIT On-Stack Replacement (OSR) — comprehensive tests.
 * Each static method returns an int: expected value on success.
 *
 * Tests exercise long-running loops that should trigger OSR (back-edge counter
 * reaches 1000), then continue executing correctly in JIT-compiled code.
 * The interpreter starts running the method; once the hot loop is detected,
 * OSR compiles the method and transfers execution to native code mid-loop.
 */
public class OsrComplete {

    // -----------------------------------------------------------------------
    // 1. osr_simple_sum — basic sum loop, triggers OSR then completes
    // -----------------------------------------------------------------------
    public static int osr_simple_sum() {
        int sum = 0;
        for (int i = 0; i < 5000; i++) {
            sum += 1;
        }
        return sum; // expect 5000
    }

    // -----------------------------------------------------------------------
    // 2. osr_accumulator — accumulate i values
    // -----------------------------------------------------------------------
    public static int osr_accumulator() {
        int sum = 0;
        for (int i = 1; i <= 2000; i++) {
            sum += i;
        }
        // 2000 * 2001 / 2 = 2001000
        return sum == 2001000 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 3. osr_while_loop — while loop variant
    // -----------------------------------------------------------------------
    public static int osr_while_loop() {
        int count = 0;
        int i = 0;
        while (i < 3000) {
            count++;
            i++;
        }
        return count == 3000 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 4. osr_countdown — backward counting loop
    // -----------------------------------------------------------------------
    public static int osr_countdown() {
        int sum = 0;
        for (int i = 5000; i > 0; i--) {
            sum += 1;
        }
        return sum; // expect 5000
    }

    // -----------------------------------------------------------------------
    // 5. osr_multiply_accumulate — multiply + add in loop
    // -----------------------------------------------------------------------
    public static int osr_multiply_accumulate() {
        int result = 0;
        for (int i = 0; i < 2000; i++) {
            result += i * 2;
        }
        // sum(i*2, i=0..1999) = 2 * (1999*2000/2) = 3998000
        return result == 3998000 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 6. osr_nested_loop — outer loop triggers OSR
    // -----------------------------------------------------------------------
    public static int osr_nested_loop() {
        int sum = 0;
        for (int i = 0; i < 100; i++) {
            for (int j = 0; j < 50; j++) {
                sum += 1;
            }
        }
        return sum; // expect 5000
    }

    // -----------------------------------------------------------------------
    // 7. osr_branch_in_loop — loop with conditional branch
    // -----------------------------------------------------------------------
    public static int osr_branch_in_loop() {
        int even = 0;
        int odd = 0;
        for (int i = 0; i < 4000; i++) {
            if (i % 2 == 0) {
                even++;
            } else {
                odd++;
            }
        }
        return (even == 2000 && odd == 2000) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 8. osr_local_variables — multiple locals across OSR boundary
    // -----------------------------------------------------------------------
    public static int osr_local_variables() {
        int a = 10;
        int b = 20;
        int c = 30;
        int sum = 0;
        for (int i = 0; i < 2000; i++) {
            sum += a + b + c; // 60 per iteration
        }
        return sum == 120000 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 9. osr_long_arithmetic — long operations in hot loop
    // -----------------------------------------------------------------------
    public static int osr_long_arithmetic() {
        long sum = 0L;
        for (int i = 0; i < 3000; i++) {
            sum += (long) i;
        }
        // 2999 * 3000 / 2 = 4498500
        return sum == 4498500L ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 10. osr_bitwise_ops — bitwise operations in loop
    // -----------------------------------------------------------------------
    public static int osr_bitwise_ops() {
        int val = 0;
        for (int i = 0; i < 2000; i++) {
            val ^= i;
        }
        // XOR of 0..1999: predictable value
        // For n ending in ...11 (binary), xor(0..n) = 0
        // 1999 % 4 = 3, so xor(0..1999) = 0
        return val == 0 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 11. osr_array_sum — array access in hot loop
    // -----------------------------------------------------------------------
    public static int osr_array_sum() {
        int[] arr = new int[2000];
        for (int i = 0; i < 2000; i++) {
            arr[i] = i + 1;
        }
        int sum = 0;
        for (int i = 0; i < 2000; i++) {
            sum += arr[i];
        }
        // sum(1..2000) = 2000*2001/2 = 2001000
        return sum == 2001000 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 12. osr_post_loop_correctness — verify locals correct after OSR loop
    // -----------------------------------------------------------------------
    public static int osr_post_loop_correctness() {
        int x = 42;
        int y = 100;
        int sum = 0;
        for (int i = 0; i < 2000; i++) {
            sum += 1;
        }
        // After the loop, x and y should be unchanged
        return (x == 42 && y == 100 && sum == 2000) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 13. osr_method_call_after — method call works after OSR completes
    // -----------------------------------------------------------------------
    private static int helper(int a, int b) { return a + b; }

    public static int osr_method_call_after() {
        int sum = 0;
        for (int i = 0; i < 2000; i++) {
            sum += 1;
        }
        // After OSR loop, call another method
        return helper(sum, 3000); // expect 5000
    }

    // -----------------------------------------------------------------------
    // 14. osr_shift_operations — shifts in hot loop
    // -----------------------------------------------------------------------
    public static int osr_shift_operations() {
        int val = 1;
        int count = 0;
        for (int i = 0; i < 2000; i++) {
            val = (val << 1) | 1;
            val = val & 0xFFFF; // keep 16 bits
            count++;
        }
        return count == 2000 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 15. osr_comparison_loop — comparison-heavy loop
    // -----------------------------------------------------------------------
    public static int osr_comparison_loop() {
        int maxSeen = 0;
        for (int i = 0; i < 3000; i++) {
            int v = (i * 7 + 13) % 5000;
            if (v > maxSeen) {
                maxSeen = v;
            }
        }
        return maxSeen > 0 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 16. osr_do_while — do-while loop
    // -----------------------------------------------------------------------
    public static int osr_do_while() {
        int count = 0;
        int i = 0;
        do {
            count++;
            i++;
        } while (i < 3000);
        return count == 3000 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 17. osr_fibonacci_iterative — iterative fib in hot loop
    // -----------------------------------------------------------------------
    public static int osr_fibonacci_iterative() {
        int a = 0, b = 1;
        for (int i = 0; i < 2000; i++) {
            int t = (a + b) & 0x7FFFFFFF; // prevent overflow sign issues
            a = b;
            b = t;
        }
        // Just verify we got a non-zero result after 2000 iterations
        return (a != 0) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 18. osr_static_field_in_loop — static field access in loop
    // -----------------------------------------------------------------------
    private static int sCounter = 0;

    public static int osr_static_field_in_loop() {
        sCounter = 0;
        for (int i = 0; i < 2000; i++) {
            sCounter++;
        }
        return sCounter == 2000 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 19. osr_negative_step — loop with negative step
    // -----------------------------------------------------------------------
    public static int osr_negative_step() {
        int count = 0;
        for (int i = 4000; i > 0; i -= 2) {
            count++;
        }
        return count == 2000 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 20. osr_multiple_exits — loop with break (multiple exit points)
    // -----------------------------------------------------------------------
    public static int osr_multiple_exits() {
        int sum = 0;
        for (int i = 0; i < 10000; i++) {
            sum += 1;
            if (sum == 5000) {
                break;
            }
        }
        return sum; // expect 5000
    }

    // -----------------------------------------------------------------------
    // 21. osr_return_from_loop — return value computed entirely in loop
    // -----------------------------------------------------------------------
    public static int osr_return_from_loop() {
        int product = 1;
        for (int i = 1; i <= 5000; i++) {
            product = (product + i) % 10007; // modular to prevent overflow
        }
        // Deterministic result
        return (product > 0) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 22. osr_two_counters — two independent counters in one loop
    // -----------------------------------------------------------------------
    public static int osr_two_counters() {
        int a = 0;
        int b = 0;
        for (int i = 0; i < 3000; i++) {
            a += 2;
            b += 3;
        }
        return (a == 6000 && b == 9000) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 23. osr_conditional_increment — conditional increment in loop
    // -----------------------------------------------------------------------
    public static int osr_conditional_increment() {
        int count = 0;
        for (int i = 0; i < 3000; i++) {
            if (i % 3 == 0) {
                count++;
            }
        }
        return count == 1000 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 24. osr_early_exit_not_taken — loop runs to completion, no early exit
    // -----------------------------------------------------------------------
    public static int osr_early_exit_not_taken() {
        int sum = 0;
        for (int i = 0; i < 5000; i++) {
            if (i < 0) { // never taken
                return -1;
            }
            sum += 1;
        }
        return sum; // expect 5000
    }

    // -----------------------------------------------------------------------
    // 25. osr_gauss_sum — Gauss sum formula verification
    // -----------------------------------------------------------------------
    public static int osr_gauss_sum() {
        int n = 3000;
        int sum = 0;
        for (int i = 1; i <= n; i++) {
            sum += i;
        }
        int expected = n * (n + 1) / 2; // 4501500
        return sum == expected ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 26. osr_min_max_tracking — track min and max in loop
    // -----------------------------------------------------------------------
    public static int osr_min_max_tracking() {
        int min = Integer.MAX_VALUE;
        int max = Integer.MIN_VALUE;
        for (int i = 0; i < 2000; i++) {
            int v = (i * 31 + 17) % 1000;
            if (v < min) min = v;
            if (v > max) max = v;
        }
        return (min < max) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 27. osr_char_loop — character manipulation in loop
    // -----------------------------------------------------------------------
    public static int osr_char_loop() {
        int sum = 0;
        for (int i = 0; i < 2000; i++) {
            char c = (char) ('A' + (i % 26));
            sum += c;
        }
        return sum > 0 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 28. osr_modular_arithmetic — modular arithmetic in loop
    // -----------------------------------------------------------------------
    public static int osr_modular_arithmetic() {
        int val = 0;
        for (int i = 0; i < 3000; i++) {
            val = (val + i) % 997; // prime modulus
        }
        return (val >= 0 && val < 997) ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 29. osr_large_iteration — very large iteration count
    // -----------------------------------------------------------------------
    public static int osr_large_iteration() {
        int sum = 0;
        for (int i = 0; i < 100000; i++) {
            sum += 1;
        }
        return sum == 100000 ? 1 : 0; // expect 1
    }

    // -----------------------------------------------------------------------
    // 30. osr_triangular_number — compute triangular number in loop
    // -----------------------------------------------------------------------
    public static int osr_triangular_number() {
        int n = 100;
        int result = 0;
        for (int i = 1; i <= n; i++) {
            // Inner loop makes outer loop hot quickly
            for (int j = 0; j < i; j++) {
                result++;
            }
        }
        // result = 100*101/2 = 5050
        return result == 5050 ? 1 : 0; // expect 1
    }
}
