import java.util.*;

/** Does `Class.getName()` and its derivatives answer HotSpot's names?
 *
 *  The review for tagging `java/lang/Class.getName` a §1.4 `Intrinsic`, and the
 *  same argument as `ClassModuleSweep` makes for `getModule`. Real
 *  `Class.getName()` is
 *
 *      String name = this.name;
 *      return name != null ? name : initClassName();
 *
 *  so it reads a field only a VM fills -- and this VM's class mirror is not
 *  laid out like the JDK's `Class`, which the registration says in as many
 *  words: *"Override with native since JDK's Class field layout differs from
 *  our mirror layout."* Yielding to that bytecode does not answer null here, it
 *  answers the INTERNAL form:
 *
 *      HotSpot   java.lang.Object    [Ljava.lang.String;    Object
 *      yielded   java/lang/Object    [Ljava/lang/String;    java/lang/Object
 *
 *  which is worse than null, because nothing throws. It propagates into every
 *  name comparison in the JDK. MEASURED: it is why
 *  `ServiceLoader.checkCaller` fails with *"module java.base does not declare
 *  `uses`"* -- `descriptor.uses()` holds `java.nio.file.spi.FileSystemProvider`
 *  and the lookup asks for `java/nio/file/spi/FileSystemProvider`.
 *
 *  Rows cover the four shapes that differ (binary name, type name, canonical
 *  name, simple name) across the receiver kinds whose naming rules are
 *  genuinely different from each other: ordinary classes, primitives, arrays of
 *  both, nested, local, anonymous, lambda, and a hidden-ish generated shape.
 *  Nothing here prints an identity hash or anything else the two VMs may choose
 *  independently.
 */
public class ClassNameSweep {
    static int rows = 0;

    static void p(String tag, Object v) {
        System.out.println(++rows + " " + tag + " |" + v + "|");
    }

    interface Body {
        Object call() throws Throwable;
    }

    static void t(String tag, Body b) {
        Object v;
        try {
            v = b.call();
        } catch (Throwable e) {
            v = e.getClass().getName() + ": " + e.getMessage();
        }
        p(tag, v);
    }

    /** All four name shapes for one receiver, so a fix to one that breaks
     *  another cannot hide. `getSimpleName` and `getCanonicalName` are derived
     *  from `getName` in the JDK, which is exactly why they belong here: if the
     *  tag on `getName` is enough, these rows say so. */
    static String four(Class<?> c) {
        return c.getName() + " / " + c.getTypeName() + " / " + c.getCanonicalName()
                + " / " + c.getSimpleName();
    }

    static class Nested {}

    enum E { A }

    public static void main(String[] a) {
        t("Object", () -> four(Object.class));
        t("String", () -> four(String.class));
        t("FileSystemProvider", () -> four(java.nio.file.spi.FileSystemProvider.class));
        t("this probe", () -> four(ClassNameSweep.class));
        t("nested class", () -> four(Nested.class));
        t("nested interface", () -> four(Body.class));
        t("enum", () -> four(E.class));
        t("int", () -> four(int.class));
        t("void", () -> four(void.class));
        t("int[]", () -> four(int[].class));
        t("int[][]", () -> four(int[][].class));
        t("Object[]", () -> four(Object[].class));
        t("String[][]", () -> four(String[][].class));
        t("Nested[]", () -> four(Nested[].class));
        t("anonymous", () -> {
            Class<?> c = new Object() {}.getClass();
            return c.getName() + " / " + c.getCanonicalName() + " / [" + c.getSimpleName() + "]";
        });
        t("local class", () -> {
            class Local {}
            Class<?> c = Local.class;
            return c.getName() + " / " + c.getCanonicalName() + " / " + c.getSimpleName();
        });
        // A lambda's class name embeds a counter both VMs choose independently,
        // so ask only the SHAPE of it.
        t("lambda name has $$Lambda", () -> {
            String n = ((Runnable) () -> {}).getClass().getName();
            return n.contains("$$Lambda") && n.startsWith("ClassNameSweep");
        });
        t("lambda name has no slash", () -> !((Runnable) () -> {}).getClass().getName().contains("/"));

        // The property every one of the above depends on, asked directly:
        // the JDK's own name lookups are by BINARY name, with dots.
        t("no slash in any binary name", () -> {
            for (Class<?> c : new Class<?>[] {
                Object.class, String.class, Nested.class, Body.class, E.class,
                int.class, int[].class, Object[].class, String[][].class,
            }) {
                if (c.getName().indexOf('/') >= 0) {
                    return "SLASH IN " + c.getName();
                }
            }
            return true;
        });
        t("forName round trip", () -> Class.forName(Nested.class.getName()) == Nested.class);
        t("array forName round trip", () -> Class.forName(String[].class.getName()) == String[].class);
        t("descriptor uses contains getName", () -> {
            java.lang.module.ModuleDescriptor d = Object.class.getModule().getDescriptor();
            return d != null
                    && d.uses().contains(java.nio.file.spi.FileSystemProvider.class.getName());
        });
        t("canUse agrees with uses", () -> Object.class.getModule()
                .canUse(java.nio.file.spi.FileSystemProvider.class));

        System.out.println("DONE ClassNameSweep");
    }
}
