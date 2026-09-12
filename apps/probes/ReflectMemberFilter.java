import java.lang.reflect.Modifier;

/**
 * Core reflection's MEMBER FILTER: the members HotSpot hides and CratonVM does not.
 *
 * The JDK keeps two process-global maps in `jdk.internal.reflect.Reflection` --
 * `fieldFilterMap` and `methodFilterMap` -- and `Class.getDeclaredFields()` /
 * `getDeclaredMethods()` subtract them before answering. A filtered member is
 * not "inaccessible"; it is INVISIBLE: `getDeclaredField` throws
 * `NoSuchFieldException` for a field the class file plainly declares.
 *
 * That distinction is why this probe asks with `getDeclaredField` /
 * `getDeclaredMethod` and reports `filtered` for the not-found case rather than
 * calling it an error. A VM that answers `VISIBLE` here has not failed an
 * access check -- it has never been asked one.
 *
 * # Do not read a row against `javap`
 *
 * `javap -p sun.misc.Unsafe` on the JDK 25 image prints
 * `public static sun.misc.Unsafe getUnsafe();`, and on 17 and 21 as well. The
 * member is declared, is public, and is still invisible to core reflection at
 * runtime. **The image is not the authority on what reflection answers**, which
 * is the whole reason this file exists as a runtime probe.
 *
 * # Why the counts are printed too
 *
 * A per-member row says which members were checked; the counts say whether
 * anything ELSE is filtered that this list does not name. `java.lang.Class`
 * reads 21 fields on HotSpot and 26 here, so the five-row gap is larger than
 * the one `classLoader` row below accounts for, and a fix that satisfies only
 * the named rows would leave the counts apart.
 *
 * Deterministic, no timing, no addresses. Check the `rows` trailer before
 * believing a clean diff.
 */
public class ReflectMemberFilter {
    static int rows;

    static void say(String s) {
        rows++;
        System.out.println(s);
    }

    /** `filtered` is the EXPECTED answer for every row in the JDK's two maps. */
    static void member(String kind, String cls, String name) {
        String r;
        try {
            Class<?> c = Class.forName(cls);
            if (kind.equals("F")) {
                c.getDeclaredField(name);
            } else {
                c.getDeclaredMethod(name);
            }
            r = "VISIBLE";
        } catch (NoSuchFieldException | NoSuchMethodException e) {
            r = "filtered";
        } catch (ClassNotFoundException e) {
            r = "no-such-class";
        } catch (Throwable t) {
            r = "ERR:" + t.getClass().getSimpleName();
        }
        say(kind + " " + cls + "." + name + " -> " + r);
    }

    static void counts(String cls) {
        try {
            Class<?> c = Class.forName(cls);
            say("count " + cls
                    + " methods=" + c.getDeclaredMethods().length
                    + " fields=" + c.getDeclaredFields().length);
        } catch (Throwable t) {
            say("count " + cls + " -> ERR:" + t.getClass().getSimpleName());
        }
    }

    public static void main(String[] args) {
        // `methodFilterMap`. One entry, and it is the door the JDK closed on
        // reflective acquisition of Unsafe -- `theUnsafe` the FIELD stays
        // reachable, which is why every real-world snippet uses the field.
        member("M", "sun.misc.Unsafe", "getUnsafe");

        // `fieldFilterMap`, the entries a supported image is expected to carry.
        member("F", "java.lang.Class", "classLoader");
        member("F", "java.lang.Class", "classData");
        member("F", "java.lang.System", "security");
        member("F", "jdk.internal.reflect.Reflection", "fieldFilterMap");
        member("F", "jdk.internal.reflect.Reflection", "methodFilterMap");
        member("F", "java.lang.invoke.MethodHandles$Lookup", "allowedModes");
        member("F", "java.lang.invoke.MethodHandles$Lookup", "lookupClass");
        member("F", "java.lang.ClassLoader", "classes");
        member("F", "java.lang.Module", "loader");

        // A CONTROL: a field nothing filters. Without it, a VM that answered
        // `filtered` for everything -- a broken `getDeclaredField` -- would
        // score as a perfect match on every row above.
        member("F", "java.lang.String", "value");
        member("M", "java.lang.String", "length");

        // The counts catch whatever the list above does not name.
        counts("sun.misc.Unsafe");
        counts("java.lang.Class");
        counts("jdk.internal.reflect.Reflection");
        counts("java.lang.invoke.MethodHandles$Lookup");
        counts("java.lang.ClassLoader");

        // `Modifier` is imported so a reader can extend a row to ask about the
        // member's flags; referenced here so the import is not stripped.
        say("modifiersAreReadable=" + Modifier.isPublic(Modifier.PUBLIC));

        System.out.println("rows " + rows);
        System.out.println("DONE ReflectMemberFilter");
    }
}
