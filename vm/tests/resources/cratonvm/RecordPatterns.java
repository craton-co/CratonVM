// JAVA21+
package cratonvm;

/**
 * Phase 83.2: Record patterns — deconstruct records via pattern matching.
 *
 * Tests:
 *   - Simple record pattern: case Point(int x, int y)
 *   - Nested record pattern: case Line(Point p1, Point p2)
 *   - Record pattern with guard
 *   - Exhaustiveness (default not needed when all cases covered)
 *   - Null in record pattern switch
 */
public class RecordPatterns {

    record Point(int x, int y) {}
    record Line(Point start, Point end) {}
    record Box(Object value) {}

    // 83.2: Simple record deconstruction
    public static int testSimpleRecord() {
        Object obj = new Point(3, 4);
        return switch (obj) {
            case Point(int x, int y) -> x + y;  // 7
            default -> -1;
        };
    }

    // 83.2: Nested record patterns
    public static int testNestedRecord() {
        Object obj = new Line(new Point(1, 2), new Point(3, 4));
        return switch (obj) {
            case Line(Point(int x1, int y1), Point(int x2, int y2))
                -> x1 + y1 + x2 + y2;  // 10
            default -> -1;
        };
    }

    // 83.2: Record pattern with guard
    public static int testRecordWithGuard() {
        Object obj = new Point(5, 10);
        return switch (obj) {
            case Point(int x, int y) when x + y > 20 -> 1;
            case Point(int x, int y) when x + y > 10 -> 2;
            case Point(int x, int y)                  -> 3;  // x+y=15, so case 2
            default -> -1;
        };
    }

    // 83.2: Null in record pattern switch
    public static int testRecordNull() {
        Object obj = null;
        return switch (obj) {
            case Point(int x, int y) -> x + y;
            case null                -> 77;
            default                  -> -1;
        };
    }

    // 83.2: Record inside Box (generic record pattern)
    public static int testRecordInBox() {
        Object obj = new Box(Integer.valueOf(42));
        return switch (obj) {
            case Box(Integer i) -> i;       // 42
            case Box(String s)  -> -2;
            default             -> -1;
        };
    }
}
