import java.lang.reflect.AccessibleObject;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * A paired probe for JEP 403 strong encapsulation as {@code setAccessible} sees
 * it, diffed against the host JDK.
 *
 * The gate is not "is the package opened". {@code AccessibleObject
 * .checkCanSetAccessible} asks four questions in order — same module, caller is
 * java.base, target module unnamed, package opened — and then a fifth that is
 * easy to miss: a **public** member of a **public** class in a merely
 * *exported* package is allowed with no {@code --add-opens} at all. A gate that
 * stops at {@code opens} is wrong on that fifth question, and it is the common
 * case, so every DENY line below is paired with an ALLOW line that differs only
 * in the member's modifiers.
 *
 * Every line prints the outcome as a value, never a verdict, so the two
 * transcripts can be diffed directly. Run under HotSpot first; its output is
 * the expected file.
 */
public class SetAccessibleModuleProbe {

    static String outcome(Runnable r) {
        try {
            r.run();
            return "OK";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static void field(String label, Class<?> c, String name) {
        System.out.println("SA field " + label + "=" + outcome(() -> {
            try {
                Field f = c.getDeclaredField(name);
                f.setAccessible(true);
            } catch (NoSuchFieldException e) {
                throw new RuntimeException(e);
            }
        }));
    }

    static void method(String label, Class<?> c, String name, Class<?>... params) {
        System.out.println("SA method " + label + "=" + outcome(() -> {
            try {
                Method m = c.getDeclaredMethod(name, params);
                m.setAccessible(true);
            } catch (NoSuchMethodException e) {
                throw new RuntimeException(e);
            }
        }));
    }

    static void ctor(String label, Class<?> c, Class<?>... params) {
        System.out.println("SA ctor " + label + "=" + outcome(() -> {
            try {
                Constructor<?> k = c.getDeclaredConstructor(params);
                k.setAccessible(true);
            } catch (NoSuchMethodException e) {
                throw new RuntimeException(e);
            }
        }));
    }

    // --- the probe's own types: unnamed module, so nothing is encapsulated ---
    public static class Own {
        private int hidden;
        public int shown;
        private Own(int x) { hidden = x; }
        public Own() { }
        private int priv() { return hidden; }
        public int pub() { return shown; }
    }

    public static void main(String[] args) throws Exception {
        // 1. java.base, package NOT opened, member private -> denied.
        field("ThreadGroup.parent", ThreadGroup.class, "parent");
        field("ThreadGroup.name", ThreadGroup.class, "name");
        field("ThreadGroup.maxPriority", ThreadGroup.class, "maxPriority");
        field("String.hash", String.class, "hash");
        field("Integer.value", Integer.class, "value");
        field("ArrayList.elementData", java.util.ArrayList.class, "elementData");

        // 2. Same packages, but a PUBLIC member of a PUBLIC class. Exported is
        //    enough; HotSpot allows these with no --add-opens.
        field("Integer.MAX_VALUE(pub static)", Integer.class, "MAX_VALUE");
        method("String.length(pub)", String.class, "length");
        method("ThreadGroup.getName(pub)", ThreadGroup.class, "getName");
        method("ArrayList.size(pub)", java.util.ArrayList.class, "size");
        ctor("ArrayList()(pub)", java.util.ArrayList.class);
        ctor("Object()(pub)", Object.class);

        // 3. Non-public members of the same exported packages -> denied, and
        //    this is the pair that proves §2 is not blanket permissiveness.
        method("String.isLatin1(priv)", String.class, "isLatin1");
        method("Integer.toUnsignedString0(privstatic)", Integer.class,
                "toUnsignedString0", int.class, int.class);
        ctor("Integer(int)(pub)", Integer.class, int.class);

        // 4. jdk.internal.misc is neither exported nor opened -> denied even
        //    for public members.
        try {
            Class<?> unsafe = Class.forName("jdk.internal.misc.Unsafe");
            method("Unsafe.getUnsafe(pub static)", unsafe, "getUnsafe");
            field("Unsafe.theUnsafe(privstatic)", unsafe, "theUnsafe");
        } catch (ClassNotFoundException e) {
            System.out.println("SA method Unsafe.getUnsafe(pub static)=NO_CLASS");
            System.out.println("SA field Unsafe.theUnsafe(privstatic)=NO_CLASS");
        }

        // 5. The classpath's own classes are in the unnamed module: nothing to
        //    encapsulate, so even private members are reachable. This is the
        //    control — a gate that denied here would break every framework.
        field("Own.hidden(priv)", Own.class, "hidden");
        field("Own.shown(pub)", Own.class, "shown");
        method("Own.priv(priv)", Own.class, "priv");
        ctor("Own(int)(priv)", Own.class, int.class);

        // 6. setAccessible(false) must never throw, whatever the target.
        System.out.println("SA clear ThreadGroup.parent=" + outcome(() -> {
            try {
                Field f = ThreadGroup.class.getDeclaredField("parent");
                f.setAccessible(false);
            } catch (NoSuchFieldException e) {
                throw new RuntimeException(e);
            }
        }));

        // 7. A denied setAccessible must leave the override flag clear, and the
        //    subsequent read must still fail. A gate that throws but writes the
        //    flag anyway would be worse than no gate.
        Field f = ThreadGroup.class.getDeclaredField("parent");
        try {
            f.setAccessible(true);
        } catch (Throwable ignored) {
            // expected on a conforming VM
        }
        System.out.println("SA afterDenied isAccessible="
                + f.canAccess(Thread.currentThread().getThreadGroup()));
        System.out.println("SA afterDenied get=" + outcome(() -> {
            try {
                f.get(Thread.currentThread().getThreadGroup());
            } catch (IllegalAccessException e) {
                throw new RuntimeException(e);
            }
        }));

        // 8. AccessibleObject.setAccessible(AccessibleObject[], boolean) is a
        //    second entry point into the same gate.
        System.out.println("SA bulk=" + outcome(() -> {
            try {
                AccessibleObject[] arr = { ThreadGroup.class.getDeclaredField("parent") };
                AccessibleObject.setAccessible(arr, true);
            } catch (NoSuchFieldException e) {
                throw new RuntimeException(e);
            }
        }));

        // 9. The Spring-CGLIB edge. `ReflectUtils` reflects
        //    `ClassLoader.defineClass` to define generated proxies; on HotSpot
        //    that needs --add-opens java.base/java.lang=ALL-UNNAMED, and
        //    without it every CGLIB class dies with "No compatible defineClass
        //    mechanism detected". The whole Spring suite turns on this one
        //    line, so it is asked here rather than left to a suite run.
        method("ClassLoader.defineClass(protected)", ClassLoader.class, "defineClass",
                String.class, byte[].class, int.class, int.class,
                java.security.ProtectionDomain.class);

        System.out.println("SA-COMPLETE");
    }
}
