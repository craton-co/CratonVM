package cratonvm;

/**
 * Session 32: JIT Escape Analysis — tests for scalar replacement of non-escaping objects.
 */
public class EscapeAnalysisTest {

    // Simple Point class with two int fields
    static class Point {
        int x;
        int y;
    }

    // Point with 3 fields
    static class Point3D {
        int x;
        int y;
        int z;
    }

    // Wrapper for a single int
    static class IntBox {
        int value;
    }

    /**
     * Basic scalar replacement: create Point, set fields, read back.
     * The Point never escapes → fields live in the JIT frame.
     * Expected: 30
     */
    public static int testPointSum() {
        Point p = new Point();
        p.x = 10;
        p.y = 20;
        return p.x + p.y;
    }

    /**
     * Scalar replacement with field reads used in computation.
     * Expected: 500 (10*10 + 20*20)
     */
    public static int testPointDistanceSquared() {
        Point p = new Point();
        p.x = 10;
        p.y = 20;
        return p.x * p.x + p.y * p.y;
    }

    /**
     * Multiple scalar-replaced objects in the same method.
     * Expected: 60 (10+20+30)
     */
    public static int testMultipleObjects() {
        IntBox a = new IntBox();
        IntBox b = new IntBox();
        IntBox c = new IntBox();
        a.value = 10;
        b.value = 20;
        c.value = 30;
        return a.value + b.value + c.value;
    }

    /**
     * 3D point scalar replacement.
     * Expected: 600 (100+200+300)
     */
    public static int testPoint3D() {
        Point3D p = new Point3D();
        p.x = 100;
        p.y = 200;
        p.z = 300;
        return p.x + p.y + p.z;
    }

    /**
     * Overwrite a field and read the new value.
     * Expected: 99
     */
    public static int testFieldOverwrite() {
        IntBox box = new IntBox();
        box.value = 42;
        box.value = 99;
        return box.value;
    }

    /**
     * Verify that escaping objects still work correctly (no scalar replacement).
     * The object escapes via return → must be heap-allocated.
     * Expected: 42
     */
    public static int testEscapingObject() {
        IntBox box = createBox(42);
        return box.value;
    }

    private static IntBox createBox(int v) {
        IntBox box = new IntBox();
        box.value = v;
        return box;
    }

    /**
     * Scalar replacement in a loop: the object is created and consumed
     * within each iteration (no escape across iterations).
     * Expected: 285 (sum of i*i for i=0..9: 0+1+4+9+16+25+36+49+64+81)
     */
    public static int testLoopLocalObject() {
        int sum = 0;
        for (int i = 0; i < 10; i++) {
            IntBox box = new IntBox();
            box.value = i * i;
            sum += box.value;
        }
        return sum;
    }

    /**
     * Default field values are zero (scalar replacement must zero-init).
     * Expected: 0
     */
    public static int testDefaultZero() {
        IntBox box = new IntBox();
        return box.value;
    }
}
