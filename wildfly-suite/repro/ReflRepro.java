// Minimal repro for WildFly bug #2: JUnit-platform discovery reflection returns
// null / wrong-typed members under JIT (B-on). Surfaces in the full suite as
// "annotationType must not be null" / "Member must not be null" /
// "Cannot invoke get/length on null" + WARN "NoSuchMethodError java/lang/Object.getName()".
// Mirrors org.junit.platform.commons.util.ReflectionUtils.streamFields /
// AnnotationUtils.findAnnotation: iterate declared fields/methods and call
// getName()/getModifiers()/isSynthetic()/getType() on each, plus findAnnotation.
import java.lang.reflect.*;
import java.lang.annotation.*;

public class ReflRepro {
    @Retention(RetentionPolicy.RUNTIME) @interface Marker { String value(); }

    @Marker("a") static int sa;
    @Marker("b") int ib;
    static long sl;
    String s;
    Object[] arr;
    @Marker("c") double dd;

    // Mimics ReflectionUtils.isStatic(Member) + name/type access in a hot filter.
    static String describeField(Field f) {
        // getName() -> the call that surfaced as NoSuchMethodError Object.getName()
        String n = f.getName();
        boolean st = Modifier.isStatic(f.getModifiers());
        boolean syn = f.isSynthetic();
        Class<?> t = f.getType();
        Marker m = f.getAnnotation(Marker.class); // findAnnotation-style
        return n + ":" + st + ":" + syn + ":" + t.getName() + ":" + (m == null ? "-" : m.value());
    }

    static String scan(Class<?> c) {
        StringBuilder sb = new StringBuilder();
        for (Field f : c.getDeclaredFields()) {
            if (f.isSynthetic()) continue;
            sb.append(describeField(f)).append(';');
        }
        for (Method me : c.getDeclaredMethods()) {
            if (me.isSynthetic()) continue;
            sb.append(me.getName()).append('(').append(me.getParameterCount()).append(')').append(';');
        }
        return sb.toString();
    }

    public static void main(String[] a) {
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        String ref = scan(ReflRepro.class);
        long ok = 0, bad = 0; String firstBad = null;
        for (int i = 0; i < iters; i++) {
            String r;
            try {
                r = scan(ReflRepro.class);
            } catch (Throwable t) {
                bad++;
                if (firstBad == null) firstBad = t.getClass().getName() + ": " + t.getMessage();
                continue;
            }
            if (r.equals(ref)) ok++;
            else { bad++; if (firstBad == null) firstBad = "MISMATCH: " + r; }
        }
        System.out.println("ref=" + ref);
        System.out.println("done iters=" + iters + " ok=" + ok + " bad=" + bad);
        if (firstBad != null) System.out.println("firstBad=" + firstBad);
    }
}
