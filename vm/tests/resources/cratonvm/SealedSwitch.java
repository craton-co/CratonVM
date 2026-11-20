// JAVA21+
package cratonvm;

/**
 * Phase 83.4: Sealed class exhaustiveness in pattern matching switch.
 *
 * Tests:
 *   - Exhaustive switch over sealed hierarchy (no default needed)
 *   - Missing case with default fallback
 *   - Switch with default on sealed type
 */
public class SealedSwitch {

    sealed interface Shape permits Circle, Rect, Triangle {}
    record Circle(int radius) implements Shape {}
    record Rect(int w, int h) implements Shape {}
    record Triangle(int base, int height) implements Shape {}

    // 83.4: Exhaustive switch — all permitted subclasses covered
    public static int testExhaustive() {
        Shape s = new Circle(5);
        return switch (s) {
            case Circle c   -> c.radius();          // 5
            case Rect r     -> r.w() * r.h();
            case Triangle t -> t.base() * t.height() / 2;
        };
    }

    // 83.4: Exhaustive with Rect
    public static int testExhaustiveRect() {
        Shape s = new Rect(3, 4);
        return switch (s) {
            case Circle c   -> c.radius();
            case Rect r     -> r.w() * r.h();       // 12
            case Triangle t -> t.base() * t.height() / 2;
        };
    }

    // 83.4: Switch with default on sealed type (allowed, default never reached)
    public static int testWithDefault() {
        Shape s = new Triangle(6, 4);
        return switch (s) {
            case Circle c   -> c.radius();
            case Rect r     -> r.w() * r.h();
            case Triangle t -> t.base() * t.height() / 2;  // 12
            default         -> -1;
        };
    }
}
