import java.lang.reflect.Field;
import java.lang.reflect.Method;
import org.springframework.test.web.Person;

/**
 * Settles WHICH enumeration order actually drives the JSON property order the
 * Spring MVC sample tests assert on.
 *
 * The 2026-08-26 3-GC page blames Class.getDeclaredMethods() order. On its own
 * numbers that does not follow: both VMs list getSomeDouble BEFORE
 * isSomeBoolean, yet HotSpot's asserted JSON is name,someBoolean,someDouble and
 * CratonVM's is name,someDouble,someBoolean. So either the field order differs
 * too (the page says it does not), or the serializer is not reading the order
 * the page thinks it is.
 *
 * Prints all four observables so one run on each VM decides it.
 */
public class PersonOrderProbe {

    public static void main(String[] args) throws Exception {
        System.out.println("== getDeclaredMethods ==");
        for (Method m : Person.class.getDeclaredMethods()) {
            System.out.println("  " + m.getName());
        }

        System.out.println("== getDeclaredFields ==");
        for (Field f : Person.class.getDeclaredFields()) {
            System.out.println("  " + f.getName());
        }

        Person p = new Person("Joe");

        // Jackson 3 (tools.jackson) -- what Spring 7 actually uses.
        System.out.println("== jackson3 ==");
        try {
            Class<?> mapperCls = Class.forName("tools.jackson.databind.ObjectMapper");
            Object mapper = mapperCls.getDeclaredConstructor().newInstance();
            Method w = mapperCls.getMethod("writeValueAsString", Object.class);
            System.out.println("  " + w.invoke(mapper, p));
        } catch (Throwable t) {
            System.out.println("  UNAVAILABLE " + t);
        }

        // Jackson 2 (com.fasterxml), for contrast.
        System.out.println("== jackson2 ==");
        try {
            Class<?> mapperCls = Class.forName("com.fasterxml.jackson.databind.ObjectMapper");
            Object mapper = mapperCls.getDeclaredConstructor().newInstance();
            Method w = mapperCls.getMethod("writeValueAsString", Object.class);
            System.out.println("  " + w.invoke(mapper, p));
        } catch (Throwable t) {
            System.out.println("  UNAVAILABLE " + t);
        }

        // What the JDK's own introspector thinks, as a third independent
        // consumer of the same reflection data.
        System.out.println("== java.beans.Introspector ==");
        try {
            java.beans.BeanInfo bi = java.beans.Introspector.getBeanInfo(Person.class);
            for (java.beans.PropertyDescriptor pd : bi.getPropertyDescriptors()) {
                System.out.println("  " + pd.getName());
            }
        } catch (Throwable t) {
            System.out.println("  UNAVAILABLE " + t);
        }
    }
}
