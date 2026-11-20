package cratonvm;

/**
 * Session 31: JIT Method Inlining — comprehensive tests.
 * Each static method returns an int: expected value on success.
 *
 * Tests exercise small methods that qualify for inlining (< 35 bytecodes):
 * getters, setters, arithmetic helpers, constant methods, branching methods,
 * field access, static field access, and chain inlining.
 */
public class InlineComplete {

    // Helper fields for getter/setter tests
    private int value;
    private long longValue;
    private int x;
    private int y;

    // Static field for static accessor tests
    private static int sCounter = 0;
    private static long sLongVal = 0L;

    // --- Tiny methods that should be inlined ---

    private static int constFive() { return 5; }
    private static int constNeg() { return -1; }
    private static int identity(int x) { return x; }
    private static int addTwo(int a, int b) { return a + b; }
    private static int subTwo(int a, int b) { return a - b; }
    private static int mulTwo(int a, int b) { return a * b; }
    private static int negate(int x) { return -x; }
    private static long addLong(long a, long b) { return a + b; }
    private static int square(int x) { return x * x; }
    private static int doubleIt(int x) { return x + x; }
    private static int incr(int x) { return x + 1; }
    private static int decr(int x) { return x - 1; }
    private static int max2(int a, int b) { return a > b ? a : b; }
    private static int min2(int a, int b) { return a < b ? a : b; }
    private static int abs(int x) { return x < 0 ? -x : x; }
    private static int clamp(int x, int lo, int hi) { return x < lo ? lo : (x > hi ? hi : x); }
    private static int bitAnd(int a, int b) { return a & b; }
    private static int bitOr(int a, int b) { return a | b; }
    private static int bitXor(int a, int b) { return a ^ b; }
    private static int shl(int x, int n) { return x << n; }
    private static int shr(int x, int n) { return x >> n; }

    private int getValue() { return this.value; }
    private void setValue(int v) { this.value = v; }
    private int getX() { return this.x; }
    private int getY() { return this.y; }

    private static void setCounter(int v) { sCounter = v; }
    private static int getCounter() { return sCounter; }

    // -----------------------------------------------------------------------
    // 1. inline_const — constant-returning method
    // -----------------------------------------------------------------------
    public static int inline_const() {
        return constFive(); // expect 5
    }

    // -----------------------------------------------------------------------
    // 2. inline_const_neg — negative constant
    // -----------------------------------------------------------------------
    public static int inline_const_neg() {
        return constNeg() + 2; // expect 1
    }

    // -----------------------------------------------------------------------
    // 3. inline_identity — pass-through
    // -----------------------------------------------------------------------
    public static int inline_identity() {
        return identity(42); // expect 42
    }

    // -----------------------------------------------------------------------
    // 4. inline_add — simple addition
    // -----------------------------------------------------------------------
    public static int inline_add() {
        return addTwo(17, 25); // expect 42
    }

    // -----------------------------------------------------------------------
    // 5. inline_sub — subtraction
    // -----------------------------------------------------------------------
    public static int inline_sub() {
        return subTwo(50, 8); // expect 42
    }

    // -----------------------------------------------------------------------
    // 6. inline_mul — multiplication
    // -----------------------------------------------------------------------
    public static int inline_mul() {
        return mulTwo(6, 7); // expect 42
    }

    // -----------------------------------------------------------------------
    // 7. inline_negate — negation
    // -----------------------------------------------------------------------
    public static int inline_negate() {
        return negate(-42); // expect 42
    }

    // -----------------------------------------------------------------------
    // 8. inline_chain — chain multiple inlined calls
    // -----------------------------------------------------------------------
    public static int inline_chain() {
        int a = addTwo(10, 20);  // 30
        int b = addTwo(a, 12);   // 42
        return b; // expect 42
    }

    // -----------------------------------------------------------------------
    // 9. inline_nested — nested inlined calls as arguments
    // -----------------------------------------------------------------------
    public static int inline_nested() {
        return addTwo(addTwo(10, 20), addTwo(5, 7)); // 30 + 12 = 42
    }

    // -----------------------------------------------------------------------
    // 10. inline_square — x * x pattern
    // -----------------------------------------------------------------------
    public static int inline_square() {
        return square(6); // expect 36
    }

    // -----------------------------------------------------------------------
    // 11. inline_double — x + x
    // -----------------------------------------------------------------------
    public static int inline_double() {
        return doubleIt(21); // expect 42
    }

    // -----------------------------------------------------------------------
    // 12. inline_incr_decr — increment then decrement
    // -----------------------------------------------------------------------
    public static int inline_incr_decr() {
        int a = incr(41);  // 42
        int b = decr(a);   // 41
        return addTwo(a, b); // 42 + 41 = 83
    }

    // -----------------------------------------------------------------------
    // 13. inline_max — branch in inlined method
    // -----------------------------------------------------------------------
    public static int inline_max() {
        return max2(17, 42); // expect 42
    }

    // -----------------------------------------------------------------------
    // 14. inline_min
    // -----------------------------------------------------------------------
    public static int inline_min() {
        return min2(42, 100); // expect 42
    }

    // -----------------------------------------------------------------------
    // 15. inline_abs_positive
    // -----------------------------------------------------------------------
    public static int inline_abs_positive() {
        return abs(42); // expect 42
    }

    // -----------------------------------------------------------------------
    // 16. inline_abs_negative
    // -----------------------------------------------------------------------
    public static int inline_abs_negative() {
        return abs(-42); // expect 42
    }

    // -----------------------------------------------------------------------
    // 17. inline_clamp_in_range
    // -----------------------------------------------------------------------
    public static int inline_clamp_in_range() {
        return clamp(42, 0, 100); // expect 42
    }

    // -----------------------------------------------------------------------
    // 18. inline_clamp_below
    // -----------------------------------------------------------------------
    public static int inline_clamp_below() {
        return clamp(-5, 42, 100); // expect 42
    }

    // -----------------------------------------------------------------------
    // 19. inline_clamp_above
    // -----------------------------------------------------------------------
    public static int inline_clamp_above() {
        return clamp(200, 0, 42); // expect 42
    }

    // -----------------------------------------------------------------------
    // 20. inline_bitand
    // -----------------------------------------------------------------------
    public static int inline_bitand() {
        return bitAnd(0xFF, 0x2A); // 0x2A = 42
    }

    // -----------------------------------------------------------------------
    // 21. inline_bitor
    // -----------------------------------------------------------------------
    public static int inline_bitor() {
        return bitOr(0x20, 0x0A); // 0x2A = 42
    }

    // -----------------------------------------------------------------------
    // 22. inline_bitxor
    // -----------------------------------------------------------------------
    public static int inline_bitxor() {
        return bitXor(0x6B, 0x41); // 0x6B ^ 0x41 = 0x2A = 42
    }

    // -----------------------------------------------------------------------
    // 23. inline_shift_left
    // -----------------------------------------------------------------------
    public static int inline_shift_left() {
        return shl(21, 1); // 42
    }

    // -----------------------------------------------------------------------
    // 24. inline_shift_right
    // -----------------------------------------------------------------------
    public static int inline_shift_right() {
        return shr(84, 1); // 42
    }

    // -----------------------------------------------------------------------
    // 25. inline_getter_setter — static getter/setter pattern
    // -----------------------------------------------------------------------
    public static int inline_getter_setter() {
        setCounter(42);
        return getCounter(); // expect 42
    }

    // -----------------------------------------------------------------------
    // 26. inline_multi_field — multiple static field accesses
    // -----------------------------------------------------------------------
    public static int inline_multi_field() {
        setCounter(17);
        int a = getCounter();
        setCounter(25);
        int b = getCounter();
        return addTwo(a, b); // 17 + 25 = 42
    }

    // -----------------------------------------------------------------------
    // 27. inline_static_field — static field round-trip
    // -----------------------------------------------------------------------
    public static int inline_static_field() {
        sLongVal = 42L;
        setCounter((int) sLongVal);
        return getCounter(); // expect 42
    }

    // -----------------------------------------------------------------------
    // 28. inline_long_add — long arithmetic
    // -----------------------------------------------------------------------
    public static int inline_long_add() {
        long a = 30L;
        long b = 12L;
        long r = a + b;
        return (int) r; // expect 42
    }

    // -----------------------------------------------------------------------
    // 29. inline_loop_with_inlined_body — inlined call in a loop
    // -----------------------------------------------------------------------
    public static int inline_loop_with_inlined_body() {
        int sum = 0;
        for (int i = 0; i < 42; i++) {
            sum = incr(sum);
        }
        return sum; // expect 42
    }

    // -----------------------------------------------------------------------
    // 30. inline_complex_expr — complex expression with multiple inlines
    // -----------------------------------------------------------------------
    public static int inline_complex_expr() {
        // ((5 + 7) * 3) + (10 - 4) = 36 + 6 = 42
        int a = addTwo(5, 7);       // 12
        int b = mulTwo(a, 3);       // 36
        int c = subTwo(10, 4);      // 6
        return addTwo(b, c);        // 42
    }
}
