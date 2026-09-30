package cratonvm;

import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InaccessibleObjectException;
import java.lang.reflect.Method;

/**
 * NEW-19: JCK-style tests for JPMS module access enforcement.
 *
 * `ModuleTarget` is reassigned to a synthetic named module by the
 * Rust-side test harness. `TckModule` stays in the unnamed module
 * (classpath). Under JEP 403, an unnamed accessor cannot deep-reflect
 * into a named module unless `--add-opens` has been granted.
 *
 * Each test returns `1` on the expected outcome, `0` otherwise.
 */
public class TckModule {

    // A private field that reflects into the SAME class — same module,
    // always allowed. Used as a smoke-test that the check isn't blanket-on.
    @SuppressWarnings("unused")
    private static int selfSecret = 1;

    // Helpers: look up a member by name on a Class via the plural no-arg
    // `getDeclaredFields()` / `getDeclaredMethods()` / `getDeclaredConstructors()`
    // entries — the single-name overloads are not always linked in the
    // synthetic JDK's `java.lang.Class` stub.

    private static Field findField(Class<?> cls, String name) {
        for (Field f : cls.getDeclaredFields()) {
            if (f.getName().equals(name)) return f;
        }
        return null;
    }

    private static Method findMethod(Class<?> cls, String name) {
        for (Method m : cls.getDeclaredMethods()) {
            if (m.getName().equals(name)) return m;
        }
        return null;
    }

    private static Constructor<?> findConstructor(Class<?> cls, int paramCount) {
        for (Constructor<?> c : cls.getDeclaredConstructors()) {
            if (c.getParameterCount() == paramCount) return c;
        }
        return null;
    }

    /**
     * setAccessible(true) on ModuleTarget's private field must throw
     * InaccessibleObjectException when the target is strong-encapsulated.
     */
    public static int denySetAccessibleField() {
        Field f = findField(ModuleTarget.class, "secret");
        if (f == null) return 0;
        try {
            f.setAccessible(true);
            return 0; // Expected an exception but got none
        } catch (InaccessibleObjectException e) {
            return 1;
        } catch (Throwable e) {
            return 0;
        }
    }

    /**
     * setAccessible(true) on a private method of ModuleTarget.
     */
    public static int denySetAccessibleMethod() {
        Method m = findMethod(ModuleTarget.class, "getSecret");
        if (m == null) return 0;
        try {
            m.setAccessible(true);
            return 0;
        } catch (InaccessibleObjectException e) {
            return 1;
        } catch (Throwable e) {
            return 0;
        }
    }

    /**
     * setAccessible(true) on a Constructor of ModuleTarget. The declaring
     * class lives in a strongly-encapsulated named module so the deep
     * check fires regardless of the ctor's own access flags.
     */
    public static int denySetAccessibleConstructor() {
        Constructor<?> c = findConstructor(ModuleTarget.class, 1);
        if (c == null) return 0;
        try {
            c.setAccessible(true);
            return 0;
        } catch (InaccessibleObjectException e) {
            return 1;
        } catch (Throwable e) {
            return 0;
        }
    }

    /**
     * Method.invoke on a public method of ModuleTarget should succeed even
     * without setAccessible (public members do not need deep reflection).
     */
    public static int allowPublicInvoke() {
        try {
            ModuleTarget t = new ModuleTarget();
            Method m = findMethod(ModuleTarget.class, "publicValue");
            if (m == null) return 0;
            Object result = m.invoke(t);
            if (result instanceof Integer && ((Integer) result) == 7) return 1;
            return 0;
        } catch (Throwable e) {
            return 0;
        }
    }

    /**
     * setAccessible(true) on this class's own private static field — same
     * module (unnamed), must always succeed regardless of encapsulation.
     */
    public static int allowSelfReflection() {
        Field f = findField(TckModule.class, "selfSecret");
        if (f == null) return 0;
        try {
            f.setAccessible(true);
            return 1;
        } catch (Throwable e) {
            return 0;
        }
    }
}
