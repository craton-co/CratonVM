import java.lang.annotation.*;
import java.lang.module.ModuleDescriptor;
import java.lang.reflect.*;
import java.util.*;

/** Lane 0's whole surface: `java.lang.Class`, `Module`, `ModuleLayer`,
 *  `ClassValue`, `ClassFrameInfo` and `ModuleDescriptor.Version`.
 *
 *  PURPOSE. Lane 0 owns 104 §1.4 shadows over eight classes, and retiring any
 *  of them needs precondition 4 satisfied by THIS instrument -- `invocations >
 *  0` for the triple in the probe's own run. A corpus census cannot answer it
 *  and `ClassNameSweep` only reaches ten of the 104. So every row below exists
 *  to dispatch a specific registered native, and the file is organised by the
 *  registration it targets rather than by what a user would call.
 *
 *  WHAT IS DELIBERATELY NOT HERE. Six of the 104 cannot be reached from Java
 *  at all -- `Class.getClassLoader0`, `getEnumConstantsShared`,
 *  `reflectionData`, `newReflectionData`, `setSigners`, and the three
 *  `Class$Atomic` CAS methods plus `Class$ReflectionData.<init>`. They are
 *  package-private VM/reflection plumbing invoked only from inside
 *  `java.lang.Class`'s own bytecode. Rows 60-64 reach them INDIRECTLY by
 *  driving the reflection cache, which is the only door a probe has.
 *
 *  PRINTING RULES, because a probe that prints an unstable value manufactures
 *  diffs. No identity hashes; no `Object.toString()`; every Set and array is
 *  sorted before printing; a URL is reduced to a boolean and its tail; an
 *  exception is reduced to type plus message. `Module` objects are printed by
 *  NAME, never by identity -- the two VMs construct different instances by
 *  construction -- but identity is still tested, as `==` between two answers
 *  from the SAME VM, which is what the JDK relies on.
 */
public class L0ClassModuleSurface {
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

    // ---- stable renderers -------------------------------------------------

    static String names(Class<?>[] cs) {
        if (cs == null) {
            return "null";
        }
        String[] a = new String[cs.length];
        for (int i = 0; i < cs.length; i++) {
            a[i] = cs[i].getName();
        }
        Arrays.sort(a);
        return Arrays.toString(a);
    }

    static String sorted(Collection<?> c) {
        if (c == null) {
            return "null";
        }
        List<String> l = new ArrayList<>();
        for (Object o : c) {
            l.add(String.valueOf(o));
        }
        Collections.sort(l);
        return l.toString();
    }

    /** Members are printed as sorted NAME lists: the JDK does not promise an
     *  order for `getDeclaredMethods`, so printing the array as-is would be a
     *  self-inflicted diff on both VMs. */
    static String memberNames(Member[] ms) {
        if (ms == null) {
            return "null";
        }
        String[] a = new String[ms.length];
        for (int i = 0; i < ms.length; i++) {
            a[i] = ms[i].getName();
        }
        Arrays.sort(a);
        return a.length + ":" + Arrays.toString(a);
    }

    static String anns(Annotation[] as) {
        if (as == null) {
            return "null";
        }
        String[] a = new String[as.length];
        for (int i = 0; i < as.length; i++) {
            a[i] = as[i].annotationType().getName();
        }
        Arrays.sort(a);
        return Arrays.toString(a);
    }

    // ---- receivers with genuinely different rules -------------------------

    @Retention(RetentionPolicy.RUNTIME)
    @interface Marker {
        String value();
    }

    @Marker("on-nested")
    @Deprecated
    static class Annotated implements Comparable<Annotated> {
        int field;

        Annotated() {}

        Annotated(int f) {
            field = f;
        }

        void method(int a) {}

        public int compareTo(Annotated o) {
            return 0;
        }
    }

    static class Sub extends Annotated {}

    enum E {
        A,
        B
    }

    interface Iface {}

    static class Holder {
        static class Inner {}

        Object anon = new Object() {};

        Runnable lam = () -> {};
    }

    static final class CV extends ClassValue<String> {
        int computed = 0;

        protected String computeValue(Class<?> type) {
            computed++;
            return "v:" + type.getSimpleName();
        }
    }

    public static void main(String[] a) throws Exception {
        // ============ Class: identity / naming / shape ============
        t("descriptorString Object", () -> Object.class.descriptorString());
        t("descriptorString int[]", () -> int[].class.descriptorString());
        t("descriptorString nested", () -> Annotated.class.descriptorString());
        t("getTypeName Object", () -> Object.class.getTypeName());
        t("getTypeName int[][]", () -> int[][].class.getTypeName());
        t("getSimpleName nested", () -> Annotated.class.getSimpleName());
        t("getSimpleName array", () -> Annotated[].class.getSimpleName());
        t("getCanonicalName nested", () -> Annotated.class.getCanonicalName());
        t("getCanonicalName local", () -> {
            class L {}
            return String.valueOf(L.class.getCanonicalName());
        });
        t("getCanonicalName anon", () -> String.valueOf(Holder.class.getDeclaredField("anon") != null
                ? new Object() {}.getClass().getCanonicalName() : "?"));
        t("getPackageName", () -> Annotated.class.getPackageName());
        t("getPackageName int", () -> int.class.getPackageName());
        t("getPackage name", () -> {
            Package pk = String.class.getPackage();
            return pk == null ? "null" : pk.getName();
        });
        t("getModifiers Object", () -> Modifier.toString(Object.class.getModifiers()));
        t("getModifiers iface", () -> Modifier.toString(Iface.class.getModifiers()));
        t("getModifiers final", () -> Modifier.toString(CV.class.getModifiers()));
        t("isInterface", () -> Iface.class.isInterface() + "/" + Object.class.isInterface());
        t("isArray", () -> int[].class.isArray() + "/" + int.class.isArray());
        t("isPrimitive", () -> int.class.isPrimitive() + "/" + Integer.class.isPrimitive());
        t("isEnum", () -> E.class.isEnum() + "/" + Object.class.isEnum());
        t("isAnnotation", () -> Marker.class.isAnnotation() + "/" + Iface.class.isAnnotation());
        t("isHidden", () -> Object.class.isHidden());

        // ============ Class: array / component ============
        t("arrayType", () -> int.class.arrayType().getName());
        t("arrayType of array", () -> Object[].class.arrayType().getName());
        t("componentType", () -> String.valueOf(int[].class.componentType()));
        t("componentType non-array", () -> String.valueOf(int.class.componentType()));
        t("getComponentType", () -> String.valueOf(Object[].class.getComponentType()));

        // ============ Class: casts and assignability ============
        t("cast ok", () -> Annotated.class.cast(new Sub()).getClass().getSimpleName());
        t("cast bad", () -> Annotated.class.cast("x"));
        t("asSubclass ok", () -> Sub.class.asSubclass(Annotated.class).getSimpleName());
        t("asSubclass bad", () -> Object.class.asSubclass(Annotated.class));
        t("isAssignableFrom", () -> Annotated.class.isAssignableFrom(Sub.class) + "/"
                + Sub.class.isAssignableFrom(Annotated.class));
        t("isInstance", () -> Annotated.class.isInstance(new Sub()) + "/"
                + Annotated.class.isInstance("x"));

        // ============ Class: forName family ============
        t("forName 1-arg", () -> Class.forName("java.util.ArrayList").getName());
        t("forName 3-arg", () -> Class
                .forName("java.util.HashMap", false, L0ClassModuleSurface.class.getClassLoader())
                .getName());
        t("forName missing", () -> Class.forName("no.such.Klass"));
        t("forName primitive name", () -> Class.forName("int"));
        t("forName array form", () -> Class.forName("[Ljava.lang.String;").getName());
        t("forName Module 2-arg", () -> {
            Class<?> c = Class.forName(Object.class.getModule(), "java.lang.Integer");
            return c == null ? "null" : c.getName();
        });

        // ============ Class: hierarchy and generics ============
        t("getSuperclass", () -> String.valueOf(Sub.class.getSuperclass().getName()));
        t("getSuperclass Object", () -> String.valueOf(Object.class.getSuperclass()));
        t("getGenericSuperclass", () -> String.valueOf(Sub.class.getGenericSuperclass()));
        t("getGenericInterfaces", () -> Arrays.toString(Annotated.class.getGenericInterfaces()));
        t("getTypeParameters count", () -> Map.class.getTypeParameters().length);
        t("getTypeParameters names", () -> {
            TypeVariable<?>[] tv = Map.class.getTypeParameters();
            String[] n = new String[tv.length];
            for (int i = 0; i < tv.length; i++) {
                n[i] = tv[i].getName();
            }
            return Arrays.toString(n);
        });
        t("getAnnotatedSuperclass", () -> {
            AnnotatedType at = Sub.class.getAnnotatedSuperclass();
            return at == null ? "null" : at.getType().getTypeName();
        });
        t("getAnnotatedInterfaces", () -> Annotated.class.getAnnotatedInterfaces().length);

        // ============ Class: enclosing ============
        t("getEnclosingClass nested", () -> String.valueOf(Holder.Inner.class.getEnclosingClass()));
        t("getEnclosingClass top", () -> String.valueOf(Object.class.getEnclosingClass()));
        t("getEnclosingMethod", () -> {
            class LocalInMain {}
            Method m = LocalInMain.class.getEnclosingMethod();
            return m == null ? "null" : m.getName();
        });
        t("getEnclosingConstructor", () -> String.valueOf(
                Holder.Inner.class.getEnclosingConstructor()));

        // ============ Class: enum constants ============
        t("getEnumConstants", () -> Arrays.toString(E.class.getEnumConstants()));
        t("getEnumConstants non-enum", () -> String.valueOf(Object.class.getEnumConstants()));

        // ============ Class: reflection members ============
        // These also drive the reflection CACHE, which is the only door a Java
        // probe has to `reflectionData` / `newReflectionData` / `Class$Atomic`.
        t("getDeclaredFields", () -> memberNames(Annotated.class.getDeclaredFields()));
        t("getDeclaredMethods", () -> memberNames(Annotated.class.getDeclaredMethods()));
        t("getDeclaredConstructors",
            () -> Annotated.class.getDeclaredConstructors().length);
        t("getDeclaredField", () -> Annotated.class.getDeclaredField("field").getName());
        t("getDeclaredField missing", () -> Annotated.class.getDeclaredField("nope"));
        t("getDeclaredMethod", () -> Annotated.class.getDeclaredMethod("method", int.class)
                .getName());
        t("getDeclaredConstructor", () -> Annotated.class.getDeclaredConstructor(int.class)
                .getParameterCount());
        t("getFields", () -> memberNames(Sub.class.getFields()));
        t("getMethods has toString", () -> {
            for (Method m : Object.class.getMethods()) {
                if (m.getName().equals("toString")) {
                    return true;
                }
            }
            return false;
        });
        t("getConstructors", () -> Annotated.class.getConstructors().length);
        t("getConstructor", () -> String.valueOf(
                Annotated.class.getConstructor().getParameterCount()));
        t("getField missing", () -> Sub.class.getField("nope"));
        t("getMethod", () -> Object.class.getMethod("hashCode").getName());
        // SECOND read of the same members: the JDK serves it from the cached
        // ReflectionData, so a divergence between the two answers is a cache
        // defect that one read cannot see.
        t("getDeclaredFields again", () -> memberNames(Annotated.class.getDeclaredFields()));
        t("getDeclaredMethods again", () -> memberNames(Annotated.class.getDeclaredMethods()));
        t("field copies not same", () -> Annotated.class.getDeclaredField("field")
                != Annotated.class.getDeclaredField("field"));
        t("field copies equal", () -> Annotated.class.getDeclaredField("field")
                .equals(Annotated.class.getDeclaredField("field")));

        // ============ Class: annotations ============
        t("getAnnotations", () -> anns(Annotated.class.getAnnotations()));
        t("getDeclaredAnnotations", () -> anns(Annotated.class.getDeclaredAnnotations()));
        t("getAnnotation value", () -> {
            Marker m = Annotated.class.getAnnotation(Marker.class);
            return m == null ? "null" : m.value();
        });
        t("getDeclaredAnnotation", () -> {
            Marker m = Annotated.class.getDeclaredAnnotation(Marker.class);
            return m == null ? "null" : m.value();
        });
        t("getAnnotationsByType", () -> Annotated.class.getAnnotationsByType(Marker.class).length);
        t("getDeclaredAnnotationsByType",
            () -> Annotated.class.getDeclaredAnnotationsByType(Marker.class).length);
        t("isAnnotationPresent", () -> Annotated.class.isAnnotationPresent(Deprecated.class));
        t("inherited annotation absent", () -> anns(Sub.class.getDeclaredAnnotations()));

        // ============ Class: loader, resources, security ============
        t("getClassLoader java.base is null", () -> Object.class.getClassLoader() == null);
        t("getClassLoader app non-null",
            () -> L0ClassModuleSurface.class.getClassLoader() != null);
        t("getResource absent", () -> String.valueOf(
                Object.class.getResource("/no/such/resource.txt")));
        t("getResourceAsStream absent", () -> String.valueOf(
                Object.class.getResourceAsStream("/no/such/resource.txt")));
        t("getResourceAsStream present", () -> {
            try (var in = Object.class.getResourceAsStream("/java/lang/Object.class")) {
                return in != null;
            }
        });
        t("getProtectionDomain has domain", () -> {
            java.security.ProtectionDomain pd = L0ClassModuleSurface.class.getProtectionDomain();
            return pd != null;
        });
        t("getSigners", () -> String.valueOf(Object.class.getSigners()));
        t("desiredAssertionStatus is boolean", () -> {
            boolean b = L0ClassModuleSurface.class.desiredAssertionStatus();
            return b || !b;
        });
        t("newInstance", () -> Annotated.class.newInstance().getClass().getSimpleName());
        t("newInstance abstract", () -> Number.class.newInstance());

        // ============ ClassValue ============
        t("ClassValue get computes", () -> {
            CV cv = new CV();
            String v1 = cv.get(String.class);
            String v2 = cv.get(String.class);
            return v1 + "/" + v2 + "/computed=" + cv.computed;
        });
        t("ClassValue remove recomputes", () -> {
            CV cv = new CV();
            cv.get(Integer.class);
            cv.remove(Integer.class);
            cv.get(Integer.class);
            return "computed=" + cv.computed;
        });
        t("ClassValue distinct per class", () -> {
            CV cv = new CV();
            return cv.get(String.class) + "/" + cv.get(Integer.class);
        });

        // ============ ClassFrameInfo, via StackWalker ============
        // The only Java door to `ClassFrameInfo`: RETAIN_CLASS_REFERENCE makes
        // the walker materialise one per frame.
        t("StackWalker declaringClass", () -> StackWalker
                .getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE)
                .walk(s -> s.findFirst().map(f -> f.getDeclaringClass().getName()).orElse("none")));
        t("StackWalker className", () -> StackWalker
                .getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE)
                .walk(s -> s.findFirst().map(StackWalker.StackFrame::getClassName).orElse("none")));
        t("StackWalker methodName", () -> StackWalker
                .getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE)
                .walk(s -> s.findFirst().map(StackWalker.StackFrame::getMethodName)
                        .orElse("none")));
        t("StackWalker getCallerClass", () -> StackWalker
                .getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE).getCallerClass().getName());

        // ============ Module: naming and descriptor ============
        t("Module getName java.base", () -> Object.class.getModule().getName());
        t("Module getName unnamed", () -> String.valueOf(
                L0ClassModuleSurface.class.getModule().getName()));
        t("Module isNamed", () -> Object.class.getModule().isNamed() + "/"
                + L0ClassModuleSurface.class.getModule().isNamed());
        t("Module getDescriptor name", () -> {
            ModuleDescriptor d = Object.class.getModule().getDescriptor();
            return d == null ? "null" : d.name();
        });
        t("Module getDescriptor unnamed is null",
            () -> L0ClassModuleSurface.class.getModule().getDescriptor() == null);
        t("Module getClassLoader java.base is null",
            () -> Object.class.getModule().getClassLoader() == null);
        t("Module getPackages has java.lang", () -> Object.class.getModule()
                .getPackages().contains("java.lang"));
        t("Module getLayer java.base is boot", () -> {
            ModuleLayer l = Object.class.getModule().getLayer();
            return l == null ? "null" : String.valueOf(l == ModuleLayer.boot());
        });
        t("Module identity within module", () -> Object.class.getModule()
                == String.class.getModule());
        t("Module getResourceAsStream absent", () -> String.valueOf(
                Object.class.getModule().getResourceAsStream("/no/such")));

        // ============ Module: readability, exports, opens, uses ============
        t("Module canRead self", () -> Object.class.getModule()
                .canRead(Object.class.getModule()));
        t("Module unnamed canRead java.base", () -> L0ClassModuleSurface.class.getModule()
                .canRead(Object.class.getModule()));
        t("Module isExported java.lang", () -> Object.class.getModule().isExported("java.lang"));
        t("Module isExported internal", () -> Object.class.getModule()
                .isExported("jdk.internal.misc"));
        t("Module isExported to module", () -> Object.class.getModule()
                .isExported("java.lang", L0ClassModuleSurface.class.getModule()));
        t("Module isOpen java.lang", () -> Object.class.getModule().isOpen("java.lang"));
        t("Module isOpen to module", () -> Object.class.getModule()
                .isOpen("java.lang", L0ClassModuleSurface.class.getModule()));
        t("Module canUse", () -> Object.class.getModule()
                .canUse(java.nio.file.spi.FileSystemProvider.class));
        // The MUTATORS, on the unnamed module where they are permitted: the
        // JDK's rule is that only the module itself may widen its own exports,
        // and an unnamed module is open to everything, so these are no-ops that
        // must still answer `this`.
        t("Module addExports self", () -> {
            Module m = L0ClassModuleSurface.class.getModule();
            return m.addExports("p", m) == m;
        });
        t("Module addOpens self", () -> {
            Module m = L0ClassModuleSurface.class.getModule();
            return m.addOpens("p", m) == m;
        });
        t("Module addUses", () -> {
            Module m = L0ClassModuleSurface.class.getModule();
            return m.addUses(java.nio.file.spi.FileSystemProvider.class) == m;
        });
        // TYPE ONLY, not the message: the JDK's own text for this is
        // "unnamed module @5387f9e0 != module java.base", and that hash is
        // chosen independently by each VM. Printing the message made this row
        // read as a diff when the two VMs actually AGREED -- a defect in the
        // probe, found 2026-09-10 by reading the row instead of the count.
        t("Module addExports on java.base denied", () -> {
            try {
                Object.class.getModule()
                        .addExports("jdk.internal.misc", L0ClassModuleSurface.class.getModule());
                return "PERMITTED";
            } catch (Throwable e) {
                return "threw " + e.getClass().getName();
            }
        });

        // ============ ModuleLayer ============
        t("ModuleLayer boot findModule java.base", () -> ModuleLayer.boot()
                .findModule("java.base").isPresent());
        t("ModuleLayer boot findModule missing", () -> ModuleLayer.boot()
                .findModule("no.such.module").isPresent());
        t("ModuleLayer boot has java.base", () -> {
            for (Module m : ModuleLayer.boot().modules()) {
                if ("java.base".equals(m.getName())) {
                    return true;
                }
            }
            return false;
        });
        t("ModuleLayer configuration non-null",
            () -> ModuleLayer.boot().configuration() != null);
        t("ModuleLayer boot is stable", () -> ModuleLayer.boot() == ModuleLayer.boot());

        // ============ ModuleDescriptor.Version ============
        t("Version parse toString", () -> ModuleDescriptor.Version.parse("1.2.3").toString());
        t("Version equals", () -> ModuleDescriptor.Version.parse("1.0")
                .equals(ModuleDescriptor.Version.parse("1.0")));
        t("Version hashCode agrees", () -> ModuleDescriptor.Version.parse("2.5").hashCode()
                == ModuleDescriptor.Version.parse("2.5").hashCode());
        t("Version compareTo lt", () -> {
            int c = ModuleDescriptor.Version.parse("1.0")
                    .compareTo(ModuleDescriptor.Version.parse("1.1"));
            return c < 0;
        });
        t("Version compareTo eq", () -> ModuleDescriptor.Version.parse("3.0")
                .compareTo(ModuleDescriptor.Version.parse("3.0")));
        t("Version parse invalid", () -> ModuleDescriptor.Version.parse(""));
        t("Version with qualifier", () -> ModuleDescriptor.Version.parse("1.0-ea+42").toString());

        System.out.println("DONE L0ClassModuleSurface rows=" + rows);
    }
}
