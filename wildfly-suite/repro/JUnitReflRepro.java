// Faithful repro for WildFly bug #2 using the REAL JUnit platform-commons
// reflection API (the code that fails in the suite: ReflectionUtils.streamFields /
// AnnotationSupport.findAnnotation, reached from ExtensionUtils.registerExtensions...).
// Compile/run against harness-cp.txt (junit-platform-commons on classpath).
import java.lang.annotation.*;
import java.util.*;
import java.util.function.Predicate;
import org.junit.platform.commons.support.AnnotationSupport;
import org.junit.platform.commons.support.HierarchyTraversalMode;
import org.junit.platform.commons.support.ReflectionSupport;
import org.junit.platform.commons.support.ModifierSupport;

public class JUnitReflRepro {
    @Retention(RetentionPolicy.RUNTIME) @interface Marker { String value(); }
    @Retention(RetentionPolicy.RUNTIME) @interface Reg {}

    @Reg static class Base { @Marker("x") static int sx; @Marker("y") int iy; long l; }
    @Reg static class Sub extends Base { String s; @Marker("z") double d; static Object o; }

    static String scan(Class<?> c) {
        StringBuilder sb = new StringBuilder();
        // Mirrors ExtensionUtils.registerExtensionsFromFields/StaticFields:
        // findFields with an isStatic predicate, then per-field annotation lookup.
        Predicate<java.lang.reflect.Field> p = ModifierSupport::isStatic;
        List<java.lang.reflect.Field> fields =
            ReflectionSupport.findFields(c, p, HierarchyTraversalMode.TOP_DOWN);
        for (java.lang.reflect.Field f : fields) {
            sb.append(f.getName()).append('=');
            Optional<Marker> m = AnnotationSupport.findAnnotation(f, Marker.class);
            sb.append(m.map(Marker::value).orElse("-")).append(';');
        }
        // class-level annotation lookup (findAnnotation on the class)
        sb.append("reg=").append(AnnotationSupport.findAnnotation(c, Reg.class).isPresent());
        return sb.toString();
    }

    public static void main(String[] a) {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        Class<?>[] cs = { Base.class, Sub.class };
        String ref0 = scan(cs[0]), ref1 = scan(cs[1]);
        long ok = 0, bad = 0; String firstBad = null;
        for (int i = 0; i < iters; i++) {
            try {
                String r0 = scan(cs[0]);
                String r1 = scan(cs[1]);
                if (r0.equals(ref0) && r1.equals(ref1)) ok++;
                else { bad++; if (firstBad == null) firstBad = "MISMATCH r0=" + r0 + " r1=" + r1; }
            } catch (Throwable t) {
                bad++;
                if (firstBad == null) firstBad = t.getClass().getName() + ": " + t.getMessage();
            }
        }
        System.out.println("ref0=" + ref0);
        System.out.println("ref1=" + ref1);
        System.out.println("done iters=" + iters + " ok=" + ok + " bad=" + bad);
        if (firstBad != null) System.out.println("firstBad=" + firstBad);
    }
}
