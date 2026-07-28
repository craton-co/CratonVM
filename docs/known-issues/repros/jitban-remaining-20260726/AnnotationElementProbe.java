import java.lang.annotation.*;
import java.lang.reflect.*;
import java.util.*;

// Closes recommendation #1 of the retired
// suppresswarnings-annotation-duplicate-value-bug write-up: its stated
// hypothesis was that CratonVM's own metadata for java.lang.SuppressWarnings
// reports the sole `value` element TWICE (a duplicated synthetic
// bridge/accessor), which would make javac's Check/Annotate see two `value`
// bindings. Enumerate the annotation interfaces' declared methods and print
// them; diff against HotSpot.
public class AnnotationElementProbe {
    @Retention(RetentionPolicy.RUNTIME)
    @interface Single { String value(); }

    @Retention(RetentionPolicy.RUNTIME)
    @interface Multi { String value(); int count() default 7; }

    @Single("x")
    @Multi("y")
    static class Target {}

    static void dump(Class<?> c) {
        Method[] ms = c.getDeclaredMethods();
        List<String> names = new ArrayList<>();
        for (Method m : ms) names.add(m.getName() + "/" + m.getParameterCount()
                + (m.isSynthetic() ? "(syn)" : "") + (m.isBridge() ? "(bridge)" : ""));
        Collections.sort(names);
        System.out.println(c.getName() + ".declaredMethods=" + names);
    }

    public static void main(String[] args) throws Exception {
        dump(SuppressWarnings.class);
        dump(Deprecated.class);
        dump(Retention.class);
        dump(Target.class.getAnnotation(Single.class).annotationType());
        dump(Multi.class);

        // Shorthand resolution actually works end to end.
        System.out.println("Single.value=" + Target.class.getAnnotation(Single.class).value());
        System.out.println("Multi.value=" + Target.class.getAnnotation(Multi.class).value()
                + " count=" + Target.class.getAnnotation(Multi.class).count());

        // The mechanism that actually broke: a LinkedHashSet of the elements,
        // then remove() each resolved element exactly as
        // Annotate.attributeAnnotation does.
        Set<Method> members = new LinkedHashSet<>();
        for (Method m : SuppressWarnings.class.getDeclaredMethods()) {
            if (!m.isSynthetic()) members.add(m);
        }
        System.out.println("members.size=" + members.size());
        Method value = SuppressWarnings.class.getDeclaredMethod("value");
        System.out.println("members.remove(value)=" + members.remove(value));
        System.out.println("members.sizeAfter=" + members.size());
        System.out.println("members.removeAgain=" + members.remove(value));
    }
}
