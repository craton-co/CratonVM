import java.util.*;

/** Is `Class.getModule()` semantics-preserving on this VM?
 *
 *  The question is a §1.4 classification, not a bug hunt. `Class.getModule()`
 *  is NOT `ACC_NATIVE` in the JDK -- its body is `return module;` -- so a
 *  native registered in front of it is a §1.4 shadow by the letter of the
 *  contract, and the contract's remedy is to yield to the bytecode.
 *
 *  That remedy cannot work here. `java.lang.Class.module` is
 *  `private transient Module` and NO Java code writes it: a real JVM populates
 *  it at class-definition time, through `Module.defineModule0`. Yielding
 *  therefore returns null, which is what 12 of the 108 corpus failures under
 *  `CRATONVM_ENFORCE_NATIVE_SHADOW=all` are -- a null `module`,
 *  `callerModule` or `thisModule` a frame or two later.
 *
 *  So the native is the VM doing a VM's job, which is exactly what §1.4's
 *  reviewed-`Intrinsic` exception is for. This probe is the review. An
 *  `Intrinsic` tag says "semantics-preserving", and the only way to earn that
 *  claim is to compare the answers against HotSpot -- otherwise the tag freezes
 *  whatever this VM happens to return.
 *
 *  Every row is a NAME or a boolean, never a Module's identity hash, because
 *  the object differs between VMs by construction. Identity is still tested,
 *  but as `==` between two answers from the SAME VM: the JDK compares Modules
 *  by identity (`Module` does not override `equals`), so two classes in one
 *  module must observe one object or `ObjectInputStream` round-trips break.
 */
public class ClassModuleSweep {
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

    static String nameOf(Class<?> c) {
        Module m = c.getModule();
        if (m == null) {
            return "NULL MODULE";
        }
        return m.isNamed() ? m.getName() : "<unnamed>";
    }

    public static void main(String[] a) {
        // java.base, reached several ways.
        t("Object", () -> nameOf(Object.class));
        t("String", () -> nameOf(String.class));
        t("ArrayList", () -> nameOf(ArrayList.class));
        t("ConcurrentHashMap", () -> nameOf(java.util.concurrent.ConcurrentHashMap.class));
        t("Properties", () -> nameOf(Properties.class));
        t("int.class", () -> nameOf(int.class));
        t("int[].class", () -> nameOf(int[].class));
        t("Object[].class", () -> nameOf(Object[].class));
        t("String[][].class", () -> nameOf(String[][].class));
        // A non-java.base platform module, and a JDK-internal package.
        t("java.sql.Date", () -> nameOf(java.sql.Date.class));
        t("java.util.logging.Logger", () -> nameOf(java.util.logging.Logger.class));
        // The application's own classes: unnamed module, on the class path.
        t("this probe", () -> nameOf(ClassModuleSweep.class));
        t("a nested type", () -> nameOf(Body.class));
        t("an anonymous type", () -> nameOf(new Object() {}.getClass()));
        t("a lambda's class", () -> nameOf(((Runnable) () -> {}).getClass()));

        // IDENTITY, within one VM. The JDK relies on it.
        t("Object == String module", () -> Object.class.getModule() == String.class.getModule());
        t("Object == ArrayList module",
            () -> Object.class.getModule() == ArrayList.class.getModule());
        t("this == nested module",
            () -> ClassModuleSweep.class.getModule() == Body.class.getModule());
        t("java.base != unnamed",
            () -> Object.class.getModule() != ClassModuleSweep.class.getModule());
        t("int == int[] module", () -> int.class.getModule() == int[].class.getModule());

        // Shape of the module object itself.
        t("java.base isNamed", () -> Object.class.getModule().isNamed());
        t("unnamed isNamed", () -> ClassModuleSweep.class.getModule().isNamed());
        t("unnamed getName is null", () -> ClassModuleSweep.class.getModule().getName() == null);
        t("java.base loader is null", () -> Object.class.getModule().getClassLoader() == null);
        t("java.base descriptor named", () -> {
            java.lang.module.ModuleDescriptor d = Object.class.getModule().getDescriptor();
            return d == null ? "NULL DESCRIPTOR" : d.name();
        });
        t("unnamed descriptor is null",
            () -> ClassModuleSweep.class.getModule().getDescriptor() == null);
        t("java.base isOpen of java.lang", () -> Object.class.getModule().isOpen("java.lang"));
        t("java.base isExported java.lang", () -> Object.class.getModule().isExported("java.lang"));
        t("java.base canRead itself", () -> Object.class.getModule().canRead(Object.class.getModule()));
        t("unnamed canRead java.base",
            () -> ClassModuleSweep.class.getModule().canRead(Object.class.getModule()));
        t("layer of java.base", () -> {
            ModuleLayer l = Object.class.getModule().getLayer();
            return l == null ? "NULL LAYER" : String.valueOf(l == ModuleLayer.boot());
        });
        t("layer of unnamed is null", () -> ClassModuleSweep.class.getModule().getLayer() == null);

        System.out.println("DONE ClassModuleSweep");
    }
}
