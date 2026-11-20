// JAVA21+
package cratonvm;

/**
 * Phase 83.1 & 83.3: Pattern matching in switch — type patterns, guards.
 *
 * Uses Java 21 pattern matching (JEP 441) to test:
 *   - Exact type match (Integer -> int)
 *   - Widening: Integer matched by Number pattern
 *   - Null handling in pattern switch
 *   - Guard expressions (when clause)
 *   - Guard false — fall through to next case
 */
public class PatternSwitch {

    // 83.1: Exact type match — Integer value matches `case Integer i`
    public static int testExactMatch() {
        Object obj = Integer.valueOf(42);
        return switch (obj) {
            case Integer i -> i;
            case String s  -> -1;
            default        -> -2;
        };
    }

    // 83.1: Widening — Integer matches `case Number n`
    public static int testWidening() {
        Object obj = Integer.valueOf(7);
        return switch (obj) {
            case Number n -> n.intValue() + 100;
            case String s -> -1;
            default       -> -2;
        };
    }

    // 83.1: Narrowing — switch returns specific type from Number
    // A Long matches Long pattern but not Integer pattern.
    public static int testNarrowing() {
        Object obj = Long.valueOf(5L);
        return switch (obj) {
            case Integer i -> -1;
            case Long l    -> (int)(l + 10L);
            default        -> -2;
        };
    }

    // 83.1: Out-of-range — value that doesn't match any specific pattern
    public static int testOutOfRange() {
        Object obj = Double.valueOf(3.14);
        return switch (obj) {
            case Integer i -> -1;
            case Long l    -> -2;
            case Double d  -> (int)(d * 100);  // 314
            default        -> -3;
        };
    }

    // 83.1: Null handling — null does not match any type pattern
    public static int testNull() {
        Object obj = null;
        return switch (obj) {
            case Integer i -> -1;
            case String s  -> -2;
            case null      -> 99;
            default        -> -3;
        };
    }

    // 83.3: Guard true — pattern matches and guard evaluates to true
    public static int testGuardTrue() {
        Object obj = Integer.valueOf(42);
        return switch (obj) {
            case Integer i when i > 10 -> i + 1;   // guard true: 43
            case Integer i             -> i;
            default                    -> -1;
        };
    }

    // 83.3: Guard false — pattern matches but guard is false, falls to next case
    public static int testGuardFalse() {
        Object obj = Integer.valueOf(3);
        return switch (obj) {
            case Integer i when i > 10 -> 999;     // guard false
            case Integer i             -> i + 100;  // falls here: 103
            default                    -> -1;
        };
    }

    // 83.3: Guard with side effect — guard evaluated after match
    static int sideEffectCounter = 0;
    public static int testGuardSideEffect() {
        sideEffectCounter = 0;
        Object obj = Integer.valueOf(5);
        int result = switch (obj) {
            case Integer i when guardEffect(i) -> i;
            case Integer i                     -> i + 50;
            default                            -> -1;
        };
        // guardEffect(5) returns false, so sideEffectCounter = 1, result = 55
        return result + sideEffectCounter;  // 55 + 1 = 56
    }

    private static boolean guardEffect(int v) {
        sideEffectCounter++;
        return v > 100;  // always false for our test
    }
}
