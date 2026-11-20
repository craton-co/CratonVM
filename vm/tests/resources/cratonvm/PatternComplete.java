// JAVA21+
package cratonvm;

/**
 * Session 53: Pattern Matching Completeness tests.
 *
 * Tests all aspects of JEP 441 (Pattern Matching for switch),
 * JEP 395 (Records), JEP 409 (Sealed Classes), and JEP 507
 * (Primitive Types in Patterns).
 *
 * Each test returns 1 on success, 0 on failure.
 */
public class PatternComplete {

    // Records for testing
    record Coord(int x, int y) {}
    record Pair(Object first, Object second) {}
    record Wrapper(Object value) {}

    // Sealed hierarchy for testing
    sealed interface Shape permits Circle, Rect {}
    record Circle(int radius) implements Shape {}
    record Rect(int w, int h) implements Shape {}

    // -----------------------------------------------------------------------
    // Type patterns
    // -----------------------------------------------------------------------

    /** String type pattern match. */
    public static int testStringPattern() {
        Object obj = "hello";
        return switch (obj) {
            case String s -> s.length();  // 5
            default -> -1;
        } == 5 ? 1 : 0;
    }

    /** Supertype pattern — Integer matched by Number. */
    public static int testSupertypeMatch() {
        Object obj = Integer.valueOf(42);
        return switch (obj) {
            case Number n -> n.intValue() == 42 ? 1 : 0;
            default -> 0;
        };
    }

    /** Default case when no pattern matches. */
    public static int testDefaultCase() {
        Object obj = new Object();
        return switch (obj) {
            case String s -> 0;
            case Integer i -> 0;
            default -> 1;
        };
    }

    /** Null with default — null goes to case null, not default. */
    public static int testNullVsDefault() {
        Object obj = null;
        int result = switch (obj) {
            case String s -> 0;
            case null -> 1;
            default -> 0;
        };
        return result;
    }

    /** Multiple null-safe patterns. */
    public static int testNullInMiddle() {
        Object obj = null;
        int result = switch (obj) {
            case Integer i -> 0;
            case null -> 1;
            case String s -> 0;
            default -> 0;
        };
        return result;
    }

    // -----------------------------------------------------------------------
    // Guard expressions (when clause)
    // -----------------------------------------------------------------------

    /** Guard true — match and guard both pass. */
    public static int testGuardPass() {
        Object obj = Integer.valueOf(50);
        return switch (obj) {
            case Integer i when i > 10 -> 1;
            case Integer i -> 0;
            default -> 0;
        };
    }

    /** Guard false — falls through to next case arm. */
    public static int testGuardFail() {
        Object obj = Integer.valueOf(5);
        return switch (obj) {
            case Integer i when i > 10 -> 0;
            case Integer i -> 1;    // falls here
            default -> 0;
        };
    }

    /** Multiple guards — second guard matches. */
    public static int testMultipleGuards() {
        Object obj = Integer.valueOf(15);
        int result = switch (obj) {
            case Integer i when i > 100 -> 0;
            case Integer i when i > 10 -> 1;   // matches
            case Integer i -> 0;
            default -> 0;
        };
        return result;
    }

    // -----------------------------------------------------------------------
    // Record patterns
    // -----------------------------------------------------------------------

    /** Simple record deconstruction. */
    public static int testRecordDecon() {
        Object obj = new Coord(3, 7);
        int result = switch (obj) {
            case Coord(int x, int y) -> x + y;  // 10
            default -> -1;
        };
        return result == 10 ? 1 : 0;
    }

    /** Record pattern with guard on components. */
    public static int testRecordGuard() {
        Object obj = new Coord(5, 10);
        int result = switch (obj) {
            case Coord(int x, int y) when x + y > 20 -> 0;
            case Coord(int x, int y) when x + y > 10 -> 1;
            case Coord(int x, int y) -> 0;
            default -> 0;
        };
        return result;
    }

    /** Record pattern with object component. */
    public static int testRecordObjectComponent() {
        Object obj = new Wrapper(Integer.valueOf(42));
        int result = switch (obj) {
            case Wrapper(Integer i) -> i;    // 42
            case Wrapper(String s) -> -1;
            default -> -2;
        };
        return result == 42 ? 1 : 0;
    }

    /** Nested record patterns. */
    public static int testNestedRecords() {
        Object obj = new Pair(new Coord(1, 2), new Coord(3, 4));
        int result = switch (obj) {
            case Pair(Coord(int x1, int y1), Coord(int x2, int y2))
                -> x1 + y1 + x2 + y2;  // 10
            default -> -1;
        };
        return result == 10 ? 1 : 0;
    }

    /** Null in record pattern switch returns null case value. */
    public static int testRecordNull() {
        Object obj = null;
        int result = switch (obj) {
            case Coord(int x, int y) -> 0;
            case null -> 1;
            default -> 0;
        };
        return result;
    }

    // -----------------------------------------------------------------------
    // Sealed class patterns
    // -----------------------------------------------------------------------

    /** Sealed interface exhaustive switch. */
    public static int testSealedSwitch() {
        Shape shape = new Circle(5);
        int result = switch (shape) {
            case Circle c -> c.radius();      // 5
            case Rect r -> r.w() * r.h();
        };
        return result == 5 ? 1 : 0;
    }

    /** Sealed interface — Rect branch. */
    public static int testSealedRect() {
        Shape shape = new Rect(3, 4);
        int result = switch (shape) {
            case Circle c -> c.radius();
            case Rect r -> r.w() * r.h();  // 12
        };
        return result == 12 ? 1 : 0;
    }

    /** Sealed record deconstruction. */
    public static int testSealedDecon() {
        Shape shape = new Circle(7);
        int result = switch (shape) {
            case Circle(int r) -> r * r;    // 49
            case Rect(int w, int h) -> w * h;
        };
        return result == 49 ? 1 : 0;
    }

    // -----------------------------------------------------------------------
    // instanceof pattern matching (expression)
    // -----------------------------------------------------------------------

    /** instanceof with type pattern. */
    public static int testInstanceofPattern() {
        Object obj = Integer.valueOf(42);
        if (obj instanceof Integer i) {
            return i == 42 ? 1 : 0;
        }
        return 0;
    }

    /** instanceof pattern — no match. */
    public static int testInstanceofNoMatch() {
        Object obj = "hello";
        if (obj instanceof Integer i) {
            return 0;
        }
        return 1;
    }

    /** instanceof with null. */
    public static int testInstanceofNull() {
        Object obj = null;
        if (obj instanceof Integer i) {
            return 0;  // null never matches instanceof
        }
        return 1;
    }

    /** Chained instanceof. */
    public static int testInstanceofChain() {
        Object obj = "test";
        if (obj instanceof String s && s.length() > 2) {
            return 1;
        }
        return 0;
    }

    // -----------------------------------------------------------------------
    // Mixed / integration
    // -----------------------------------------------------------------------

    /** Process a heterogeneous collection element. */
    public static int testMixedDispatch() {
        int sum = 0;
        Object[] items = { Integer.valueOf(10), "hello", Double.valueOf(3.14), null };
        for (int i = 0; i < items.length; i++) {
            sum += classify(items[i]);
        }
        // 10 + 5 + 3 + 0 = 18
        return sum == 18 ? 1 : 0;
    }

    private static int classify(Object obj) {
        return switch (obj) {
            case Integer i -> i;
            case String s -> s.length();
            case Double d -> (int) d.doubleValue();
            case null -> 0;
            default -> -1;
        };
    }

    /** Area calculation via sealed record patterns. */
    public static int testAreaCalc() {
        Shape[] shapes = { new Circle(1), new Rect(2, 3), new Circle(10) };
        int total = 0;
        for (int i = 0; i < shapes.length; i++) {
            total += area(shapes[i]);
        }
        // pi*1 + 6 + pi*100 = ~3 + 6 + ~314 = ~323
        // Using int truncation: (int)(3.14159*1) + 6 + (int)(3.14159*100) = 3 + 6 + 314 = 323
        return total == 323 ? 1 : 0;
    }

    private static int area(Shape s) {
        return switch (s) {
            case Circle(int r) -> (int)(3.14159 * r * r);
            case Rect(int w, int h) -> w * h;
        };
    }
}
