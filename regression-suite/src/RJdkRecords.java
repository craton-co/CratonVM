import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.lang.reflect.RecordComponent;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

/**
 * JDK-only corpus: records and sealed classes -- modern class-file attributes
 * ({@code Record}, {@code PermittedSubclasses}) plus reflection over them.
 *
 * A fabricated compatibility class has no {@code Record} or
 * {@code PermittedSubclasses} attribute, so this vector fails loudly if the
 * class bytes did not come from javac.
 *
 * Language level: Java 17 (records 16+, sealed classes 17+), so the whole
 * {@code --release 17/21/25} matrix compiles.
 *
 * Determinism: reflective arrays are sorted before printing.
 */
public class RJdkRecords {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** A canonical record with a compact constructor and an extra accessor. */
    public record Point(int x, int y) implements Comparable<Point> {
        public Point {
            if (x < 0 || y < 0) {
                throw new IllegalArgumentException("negative");
            }
        }

        public Point(int both) {
            this(both, both);
        }

        int manhattan() {
            return x + y;
        }

        @Override
        public int compareTo(Point o) {
            return x != o.x ? Integer.compare(x, o.x) : Integer.compare(y, o.y);
        }
    }

    /** A generic record, to exercise the generic signature of a component. */
    public record Boxed<T>(T value, String label) { }

    public sealed interface Shape permits Circle, Square, Rect { }

    public record Circle(int r) implements Shape { }

    public record Square(int side) implements Shape { }

    /** A non-record, final permitted subclass. */
    public static final class Rect implements Shape {
        final int w;
        final int h;

        Rect(int w, int h) {
            this.w = w;
            this.h = h;
        }
    }

    static void recordSemantics() {
        Point a = new Point(3, 4);
        Point b = new Point(3, 4);
        Point c = new Point(5);
        check(a.x() == 3 && a.y() == 4, "record accessors");
        check(a.equals(b) && !a.equals(c), "record equals");
        check(a.hashCode() == b.hashCode(), "record hashCode");
        check(a != b, "distinct instances");
        check(a.toString().equals("Point[x=3, y=4]"), "record toString: " + a);
        check(c.equals(new Point(5, 5)), "secondary constructor delegates to canonical");
        check(a.manhattan() == 7, "extra method");
        boolean threw = false;
        try {
            new Point(-1, 0);
        } catch (IllegalArgumentException expected) {
            threw = true;
        }
        check(threw, "compact constructor validation must run");

        List<Point> pts = new ArrayList<>(Arrays.asList(new Point(2, 9), new Point(1, 1), a));
        Collections.sort(pts);
        check(pts.get(0).equals(new Point(1, 1)), "record Comparable");

        Boxed<String> boxed = new Boxed<>("v", "l");
        check(boxed.value().equals("v") && boxed.label().equals("l"), "generic record");
        System.out.println("CK RJdkRecords points=" + pts + " boxed=" + boxed);
    }

    static void recordReflection() throws Exception {
        Class<?> pc = Point.class;
        check(pc.isRecord(), "Class.isRecord");
        check(!RJdkRecords.class.isRecord(), "non-record isRecord");
        RecordComponent[] comps = pc.getRecordComponents();
        check(comps != null && comps.length == 2, "getRecordComponents length");
        StringBuilder sb = new StringBuilder();
        for (RecordComponent rc : comps) {
            sb.append(rc.getName()).append(':').append(rc.getType().getName()).append(' ');
            Method acc = rc.getAccessor();
            check(acc.getName().equals(rc.getName()), "accessor name");
            // NOT `!= null`. A reflective read that answers a boxed 0 for a
            // field it cannot resolve -- this VM's recorded failure mode for
            // get-field-by-name -- is non-null and was green here. The value is
            // what the component is for.
            Object got = acc.invoke(new Point(7, 8));
            Object want = rc.getName().equals("x") ? Integer.valueOf(7) : Integer.valueOf(8);
            check(want.equals(got), "accessor " + rc.getName() + " returned " + got
                    + ", want " + want);
        }
        check(sb.toString().equals("x:int y:int "), "component list: " + sb);

        // The canonical constructor must be reflectively reachable and callable.
        Constructor<?> canonical = pc.getDeclaredConstructor(int.class, int.class);
        Object made = canonical.newInstance(11, 12);
        check(made.equals(new Point(11, 12)), "canonical constructor via reflection");

        // Generic component signature survives.
        RecordComponent[] bc = Boxed.class.getRecordComponents();
        check(bc.length == 2, "Boxed components");
        check(bc[0].getGenericType().getTypeName().equals("T"), "generic component type: "
                + bc[0].getGenericType().getTypeName());

        // Records are implicitly final and extend java.lang.Record.
        check(java.lang.reflect.Modifier.isFinal(pc.getModifiers()), "records are final");
        check(pc.getSuperclass() == java.lang.Record.class, "record superclass");
        System.out.println("CK RJdkRecords components=" + sb.toString().trim());
    }

    static void sealedReflection() {
        Class<?> shape = Shape.class;
        check(shape.isSealed(), "Shape must be sealed");
        check(!RJdkRecords.class.isSealed(), "non-sealed isSealed");
        Class<?>[] permitted = shape.getPermittedSubclasses();
        check(permitted != null && permitted.length == 3, "permitted subclass count");
        List<String> names = new ArrayList<>();
        for (Class<?> p : permitted) {
            names.add(p.getSimpleName());
        }
        Collections.sort(names);
        check(names.equals(Arrays.asList("Circle", "Rect", "Square")), "permitted: " + names);
        check(Circle.class.isRecord() && !Rect.class.isRecord(), "mixed permitted kinds");
        // A sealed interface still dispatches normally.
        Shape s = new Circle(2);
        check(s instanceof Circle && !(s instanceof Square), "sealed instanceof");
        int area = describe(new Circle(3)) + describe(new Square(4)) + describe(new Rect(2, 5));
        check(area == 9 + 16 + 10, "sealed dispatch sum: " + area);
        System.out.println("CK RJdkRecords permitted=" + names + " area=" + area);
    }

    /** Java-17-compatible exhaustive-ish dispatch (no pattern switch, which is 21+). */
    static int describe(Shape s) {
        if (s instanceof Circle) {
            return ((Circle) s).r() * ((Circle) s).r();
        }
        if (s instanceof Square) {
            return ((Square) s).side() * ((Square) s).side();
        }
        Rect r = (Rect) s;
        return r.w * r.h;
    }

    public static void main(String[] args) throws Exception {
        recordSemantics();
        recordReflection();
        sealedReflection();
        System.out.println("CK RJdkRecords checks=" + checks);
        System.out.println("PASS RJdkRecords (" + checks + " checks)");
    }
}
