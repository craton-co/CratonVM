import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Modifier;
import sun.reflect.ReflectionFactory;

/**
 * Same five-link walk as RFChain, but filter-immune.
 *
 * getDeclaredFields() on java.lang.reflect.Constructor returns NOTHING: the JDK
 * filters the members of java.lang.reflect.* out of core reflection, so a walk
 * built on getDeclaredFields reports every link as absent on a HEALTHY image
 * and is therefore mute. MethodHandles.privateLookupIn + findVarHandle resolves
 * the field directly and is not subject to that filter.
 *
 * Also: do NOT call setAccessible on the constructor that
 * newConstructorForSerialization returns. It is already accessible; calling it
 * again re-runs the module check against THIS caller and throws
 * InaccessibleObjectException for a package java.base does not open.
 */
public class RFChain2 {

    static final MethodHandles.Lookup LOOKUP = MethodHandles.lookup();

    static Object readVia(Object owner, String name, Class<?> declared) {
        if (owner == null) return null;
        Class<?> c = owner.getClass();
        while (c != null) {
            try {
                MethodHandles.Lookup pl = MethodHandles.privateLookupIn(c, LOOKUP);
                VarHandle vh = pl.findVarHandle(c, name, declared);
                return vh.get(owner);
            } catch (Throwable t) {
                c = c.getSuperclass();
            }
        }
        return "<UNREADABLE:" + name + ">";
    }

    static Object readField(Object owner, String name) {
        if (owner == null) return null;
        Class<?> c = owner.getClass();
        while (c != null) {
            for (Field f : c.getDeclaredFields()) {
                if (f.getName().equals(name)) {
                    try {
                        MethodHandles.Lookup pl = MethodHandles.privateLookupIn(c, LOOKUP);
                        VarHandle vh = pl.findVarHandle(c, name, f.getType());
                        return vh.get(owner);
                    } catch (Throwable t) {
                        return "<UNREADABLE:" + name + ":" + t.getClass().getSimpleName() + ">";
                    }
                }
            }
            c = c.getSuperclass();
        }
        return "<ABSENT:" + name + ">";
    }

    static String cls(Object o) {
        if (o == null) return "null";
        if (o instanceof String && ((String) o).startsWith("<")) return (String) o;
        return o.getClass().getName();
    }

    static void listFields(String label, Object o) {
        if (o == null || (o instanceof String && ((String) o).startsWith("<"))) return;
        StringBuilder sb = new StringBuilder();
        Class<?> c = o.getClass();
        while (c != null && c != Object.class) {
            for (Field f : c.getDeclaredFields()) {
                if (Modifier.isStatic(f.getModifiers())) continue;
                if (sb.length() > 0) sb.append(", ");
                sb.append(f.getName()).append(":").append(f.getType().getSimpleName());
            }
            c = c.getSuperclass();
        }
        System.out.println("        " + label + " declared fields = [" + sb + "]");
    }

    static void walk(String when, Constructor<?> ctor) {
        Object acc = readVia(ctor, "constructorAccessor", getAccessorType());
        System.out.println("    [" + when + "] LINK1/2 constructorAccessor = " + cls(acc));
        listFields("accessor", acc);
        Object tgt = readField(acc, "target");
        System.out.println("    [" + when + "] LINK3 target                = " + cls(tgt));
        listFields("target", tgt);
        Object ic = readField(tgt, "instanceClass");
        String icStr = (ic instanceof Class) ? ((Class<?>) ic).getName() : cls(ic);
        System.out.println("    [" + when + "] LINK4 instanceClass         = " + icStr);
    }

    static Class<?> getAccessorType() {
        try {
            return Class.forName("jdk.internal.reflect.ConstructorAccessor");
        } catch (Throwable t) {
            return Object.class;
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("java.version = " + System.getProperty("java.version"));
        System.out.println("java.vm.name = " + System.getProperty("java.vm.name"));
        System.out.println("Constructor.getDeclaredFields().length = "
                + Constructor.class.getDeclaredFields().length
                + "   (0 means core reflection filters them; that is the JDK, not a defect)");

        Class<?>[] targets = { Integer.class, java.util.ArrayList.class, String.class };

        for (Class<?> cl : targets) {
            System.out.println("");
            System.out.println("== target = " + cl.getName());
            ReflectionFactory rf = ReflectionFactory.getReflectionFactory();
            Constructor<?> ctor;
            try {
                ctor = rf.newConstructorForSerialization(cl);
            } catch (Throwable t) {
                System.out.println("    newConstructorForSerialization THREW " + t);
                continue;
            }
            if (ctor == null) {
                System.out.println("    newConstructorForSerialization returned null");
                continue;
            }
            System.out.println("    ctor.getDeclaringClass = " + ctor.getDeclaringClass().getName());

            walk("before", ctor);

            try {
                Object made = ctor.newInstance();
                System.out.println("    newInstance -> " + cls(made)
                        + ((made != null && made.getClass() == cl) ? "   OK" : "   WRONG"));
            } catch (Throwable t) {
                System.out.println("    newInstance THREW " + t.getClass().getName() + ": " + t.getMessage());
            }

            walk("after", ctor);
        }
    }
}
