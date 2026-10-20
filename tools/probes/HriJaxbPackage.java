import java.lang.annotation.Annotation;
import java.util.Arrays;

/**
 * Package-level annotations of a class-path package, read the way JAXB reads
 * an {@code @XmlSchema} namespace. Under --compatible {@code Package.getPackageInfo}
 * could not load a {@code package-info} class that was not loaded yet (the app
 * loader's class-path search missed), so ehcache's {@code org.ehcache.xml.model}
 * lost its namespace and Hibernate Reactive's CacheTest failed. Run with the
 * Hibernate Reactive runner class path (ehcache + jakarta.xml.bind); every row
 * must match HotSpot 25.
 */
public class HriJaxbPackage {
    static void row(String k, Object v) {
        System.out.println(k + "=" + v);
    }

    static String names(Annotation[] as) {
        return Arrays.stream(as).map(a -> a.annotationType().getName()).sorted().toList().toString();
    }

    public static void main(String[] args) throws Exception {
        String cn = args.length > 0 ? args[0] : "org.ehcache.xml.model.ConfigType";
        Class<?> c = Class.forName(cn);
        Package p = c.getPackage();
        row("package", p.getName());
        row("package.annotations", names(p.getAnnotations()));
        row("package.declared", names(p.getDeclaredAnnotations()));
        Class<? extends Annotation> xs =
                Class.forName("jakarta.xml.bind.annotation.XmlSchema").asSubclass(Annotation.class);
        Annotation a = p.getAnnotation(xs);
        row("XmlSchema", a == null ? null : xs.getMethod("namespace").invoke(a));
        row("XmlSchema.elementFormDefault", a == null ? null : xs.getMethod("elementFormDefault").invoke(a));
        row("isAnnotationPresent", p.isAnnotationPresent(xs));
        Class<?> pi = Class.forName(p.getName() + ".package-info", false, c.getClassLoader());
        row("package-info.annotations", names(pi.getAnnotations()));
        row("package-info.modifiers", java.lang.reflect.Modifier.toString(pi.getModifiers()));
        row("definedPackage", c.getClassLoader().getDefinedPackage(p.getName()) == p);
    }
}
