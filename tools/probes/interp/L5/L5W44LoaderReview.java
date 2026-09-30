// Interpreter round i1, wave 44, lane L5 -- review of the loader surface
// against HotSpot: Class.forName with initialize / loader combinations,
// ClassLoader.getDefinedPackage(s), Package.getPackage across loaders,
// ClassLoader.getUnnamedModule, and Lookup.defineClass / defineHiddenClass
// package and access checks.
//
// Run (no setup):
//   javac -d out L5W44LoaderReview.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W44LoaderReview
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   forname-noinit=[] [A] true
//   forname-null-loader-app-class=java.lang.ClassNotFoundException: L5W44LoaderReview$InitB
//   forname-null-loader-jdk=java.util.ArrayList
//   forname-array-noinit=[LL5W44LoaderReview$InitC; L5W44LoaderReview$InitC [A]
//   forname-prim=java.lang.ClassNotFoundException: int | [I | [I
//   forname-bad-array=java.lang.ClassNotFoundException: [Ljava/lang/String | java.lang.ClassNotFoundException: [[ | java.lang.ClassNotFoundException: java/lang/String
//   forname-clinit-throws=java.lang.ExceptionInInitializerError: null / cause java.lang.RuntimeException | java.lang.NoClassDefFoundError: Could not initialize class L5W44LoaderReview$Boom / cause java.lang.ExceptionInInitializerError
//   forname-noinit-then-new=false true
//   forname-user-loader=true java.lang.ClassNotFoundException: r44.Def true
//   defined-package=true true true true q44 [q44]
//   defined-package-builtin=true true true true true
//   package-getpackage=true true true
//   package-same-name-two-loaders=true t44 true
//   array-package=null null null java.lang java.lang java.lang
//   unnamed-module=true true true false null null null unnamed module @H true [u44]
//   unnamed-module-builtin=true true true true true true true
//   lookup-define=L5W44Def1 true true false
//   lookup-define-other-package=java.lang.IllegalArgumentException: v44/Def not in same package as lookup class
//   lookup-define-duplicate=java.lang.LinkageError: loader 'app' attempted duplicate class definition for L5W44Def1. (L5W44Def1 is in unnamed module of loader 'app')
//   lookup-define-no-package-access=java.lang.IllegalAccessException: Lookup does not have PACKAGE access
//   lookup-define-public-lookup=java.lang.IllegalAccessException: Lookup does not have PACKAGE access
//   lookup-define-java-lang=java.lang.IllegalAccessException: module java.base does not open java.lang to unnamed module @H
//   lookup-define-in-user-loader=w44.Def2 true true true
//   hidden-define=L5W44Hid1/0xH true true true true java.lang.ClassNotFoundException: L5W44Hid1/0xH
//   hidden-define-other-package=java.lang.IllegalArgumentException: x44/Hid not in same package as lookup class
//   hidden-define-no-package-access=java.lang.IllegalAccessException: L5W44LoaderReview/module does not have full privilege access
//   hidden-define-twice=true true true
//   hidden-nestmate=true true
//
// On the base `5248262b7` (from the code; the other rows were not traced
// to the end, and the host run decides them):
//   * `forname-bad-array`: the first column is `[Ljava.lang.String` -- the
//     array arm of `lang_class::native_class_for_name` named a descriptor
//     that is not a well-formed array in the caller's dotted spelling;
//   * `lookup-define-other-package`, `lookup-define-no-package-access`,
//     `lookup-define-public-lookup`, `hidden-define-other-package` and
//     `hidden-define-no-package-access` define the class (the first three
//     print `class ...`, the hidden ones the new Lookup): the natives that
//     stand in for `Lookup.defineClass` / `defineHiddenClass`
//     (`lookup_define.rs`) never asked the lookup's modes or the package;
//   * `lookup-define-duplicate`: `java.lang.LinkageError: loader Application
//     attempted duplicate class definition for L5W44Def1.` (the backend's
//     loader id, no module clause).
// `--compatible`: the same, except `lookup-define-duplicate`, whose HotSpot
// wording is `--jdk-only`'s only (as `ClassLoader.defineClass`'s since wave
// 38; the base's text there, by design).

import java.io.*;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodHandles.Lookup;
import java.util.*;

public class L5W44LoaderReview {
    static final String P = "L5W44LoaderReview$";
    static final ClassLoader APP = L5W44LoaderReview.class.getClassLoader();
    static final List<String> INITS = new ArrayList<>();

    static byte[] rename(byte[] bytes, Map<String, String> renames) throws IOException {
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(bytes));
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(buf);
        out.writeInt(in.readInt());
        out.writeShort(in.readUnsignedShort());
        out.writeShort(in.readUnsignedShort());
        int count = in.readUnsignedShort();
        out.writeShort(count);
        for (int i = 1; i < count; i++) {
            int tag = in.readUnsignedByte();
            out.writeByte(tag);
            switch (tag) {
                case 1 -> {
                    String s = in.readUTF();
                    for (Map.Entry<String, String> e : renames.entrySet()) s = s.replace(e.getKey(), e.getValue());
                    out.writeUTF(s);
                }
                case 3, 4 -> out.writeInt(in.readInt());
                case 5, 6 -> { out.writeLong(in.readLong()); i++; }
                case 7, 8, 16, 19, 20 -> out.writeShort(in.readUnsignedShort());
                case 9, 10, 11, 12, 17, 18 -> out.writeInt(in.readInt());
                case 15 -> { out.writeByte(in.readUnsignedByte()); out.writeShort(in.readUnsignedShort()); }
                default -> throw new IOException("tag " + tag);
            }
        }
        in.transferTo(out);
        out.flush();
        return buf.toByteArray();
    }

    static byte[] own(String n) throws IOException {
        try (InputStream in = ClassLoader.getSystemResourceAsStream(n + ".class")) { return in.readAllBytes(); }
    }

    /** The bytes of the nested `Def`, renamed to `name` (internal form). */
    static byte[] def(String name) throws IOException {
        return rename(own(P + "Def"), Map.of(P + "Def", name));
    }

    /** A loader that defines what it is handed, and delegates the rest to the application loader. */
    static final class Own extends ClassLoader {
        Own(String name) { super(name, APP); }
        Class<?> define(String binaryName, byte[] b) { return defineClass(binaryName, b, 0, b.length); }
        Package pkg(String name) { return getPackage(name); }
    }

    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            System.out.println(label + "=" + describe(t));
        }
    }

    static String describe(Throwable t) {
        String m = t.getMessage();
        String s = t.getClass().getName() + ": " + (m == null ? "null" : m.replaceAll("@[0-9a-f]+", "@H").replaceAll("0x[0-9a-f]+", "0xH"));
        if (t.getCause() != null) s += " / cause " + t.getCause().getClass().getName();
        return s;
    }

    static String attempt(Row r) {
        try {
            Object o = r.run();
            return String.valueOf(o);
        } catch (Throwable t) {
            return describe(t);
        }
    }

    public static void main(String[] args) throws Exception {
        // ---- Class.forName -------------------------------------------------
        row("forname-noinit", () -> {
            Class<?> c = Class.forName(P + "InitA", false, APP);
            String before = INITS.toString();
            Class<?> c2 = Class.forName(P + "InitA");
            return before + " " + INITS + " " + (c == c2);
        });
        row("forname-null-loader-app-class", () -> attempt(() -> Class.forName(P + "InitB", false, null)));
        row("forname-null-loader-jdk", () -> Class.forName("java.util.ArrayList", true, null).getName());
        row("forname-array-noinit", () -> {
            Class<?> a = Class.forName("[L" + P + "InitC;", true, APP);
            return a.getName() + " " + a.getComponentType().getName() + " " + INITS;
        });
        row("forname-prim", () -> attempt(() -> Class.forName("int")) + " | " + attempt(() -> Class.forName("[I").getName())
                + " | " + attempt(() -> Class.forName("[I", false, null).getName()));
        row("forname-bad-array", () -> attempt(() -> Class.forName("[Ljava.lang.String")) + " | "
                + attempt(() -> Class.forName("[[")) + " | " + attempt(() -> Class.forName("java/lang/String")));
        row("forname-clinit-throws", () -> attempt(() -> Class.forName(P + "Boom")) + " | " + attempt(() -> Class.forName(P + "Boom")));
        row("forname-noinit-then-new", () -> {
            Class<?> c = Class.forName(P + "InitD", false, APP);
            String before = String.valueOf(INITS.contains("D"));
            c.getDeclaredConstructor().newInstance();
            return before + " " + INITS.contains("D");
        });
        row("forname-user-loader", () -> {
            Own l = new Own("fl");
            Class<?> c = l.define("r44.Def", def("r44/Def"));
            return (Class.forName("r44.Def", false, l) == c) + " " + attempt(() -> Class.forName("r44.Def", false, APP))
                    + " " + (Class.forName(P + "InitE", false, l) == Class.forName(P + "InitE", false, APP));
        });

        // ---- packages ------------------------------------------------------
        row("defined-package", () -> {
            Own l = new Own("pk");
            Class<?> c = l.define("q44.Def", def("q44/Def"));
            Package p = l.getDefinedPackage("q44");
            return (p != null) + " " + (c.getPackage() == p) + " " + (APP.getDefinedPackage("q44") == null)
                    + " " + (l.pkg("q44") == p) + " " + p.getName() + " " + Arrays.stream(l.getDefinedPackages()).map(Package::getName).sorted().toList();
        });
        row("defined-package-builtin", () -> (APP.getDefinedPackage("java.lang") == null)
                + " " + (ClassLoader.getPlatformClassLoader().getDefinedPackage("java.lang") == null)
                + " " + (Object.class.getPackage() != null) + " " + (APP.getDefinedPackage("") != null)
                + " " + (L5W44LoaderReview.class.getPackage() == APP.getDefinedPackage("")));
        row("package-getpackage", () -> {
            Own l = new Own("gp");
            l.define("s44.Def", def("s44/Def"));
            @SuppressWarnings("deprecation")
            Package viaCaller = Package.getPackage("s44");
            @SuppressWarnings("deprecation")
            Package lang = Package.getPackage("java.lang");
            return (viaCaller == null) + " " + (lang != null && lang == Object.class.getPackage()) + " " + (l.pkg("java.lang") == lang);
        });
        row("package-same-name-two-loaders", () -> {
            Own a = new Own("a"), b = new Own("b");
            Class<?> ca = a.define("t44.Def", def("t44/Def"));
            Class<?> cb = b.define("t44.Def", def("t44/Def"));
            return (ca.getPackage() != cb.getPackage()) + " " + ca.getPackage().getName() + " " + (a.getDefinedPackage("t44") == ca.getPackage());
        });
        row("array-package", () -> String.valueOf(String[].class.getPackage()) + " " + int[].class.getPackage() + " " + int.class.getPackage()
                + " " + String[].class.getPackageName() + " " + int[].class.getPackageName() + " " + int.class.getPackageName());

        // ---- unnamed modules -----------------------------------------------
        row("unnamed-module", () -> {
            Own l = new Own("um");
            Class<?> c = l.define("u44.Def", def("u44/Def"));
            Module m = l.getUnnamedModule();
            return (c.getModule() == m) + " " + (m != APP.getUnnamedModule()) + " " + (m.getClassLoader() == l)
                    + " " + m.isNamed() + " " + m.getName() + " " + m.getDescriptor() + " " + m.getLayer()
                    + " " + m.toString().replaceAll("@[0-9a-f]+", "@H") + " " + (l.getUnnamedModule() == m)
                    + " " + m.getPackages();
        });
        row("unnamed-module-builtin", () -> {
            Module pm = ClassLoader.getPlatformClassLoader().getUnnamedModule();
            Module am = APP.getUnnamedModule();
            return (pm.getClassLoader() == ClassLoader.getPlatformClassLoader()) + " " + (am.getClassLoader() == APP)
                    + " " + (L5W44LoaderReview.class.getModule() == am) + " " + (pm != am) + " " + am.canRead(Object.class.getModule())
                    + " " + am.isExported("anything") + " " + am.isOpen("anything");
        });

        // ---- Lookup.defineClass / defineHiddenClass --------------------------
        Lookup lookup = MethodHandles.lookup();
        row("lookup-define", () -> {
            Class<?> c = lookup.defineClass(def("L5W44Def1"));
            return c.getName() + " " + (c.getClassLoader() == APP) + " " + (c.getPackageName().isEmpty()) + " " + c.isHidden();
        });
        row("lookup-define-other-package", () -> attempt(() -> lookup.defineClass(def("v44/Def"))));
        row("lookup-define-duplicate", () -> attempt(() -> lookup.defineClass(def("L5W44Def1"))));
        row("lookup-define-no-package-access", () -> attempt(() -> lookup.dropLookupMode(Lookup.PACKAGE).defineClass(def("L5W44Def2"))));
        row("lookup-define-public-lookup", () -> attempt(() -> MethodHandles.publicLookup().defineClass(def("L5W44Def3"))));
        row("lookup-define-java-lang", () -> attempt(() -> MethodHandles.privateLookupIn(Object.class, lookup)));
        row("lookup-define-in-user-loader", () -> {
            Own l = new Own("dl");
            Class<?> host = l.define("w44.Def", def("w44/Def"));
            Lookup in = MethodHandles.privateLookupIn(host, lookup);
            Class<?> c = in.defineClass(def("w44/Def2"));
            return c.getName() + " " + (c.getClassLoader() == l) + " " + (c.getPackage() == host.getPackage()) + " " + (c.getModule() == host.getModule());
        });
        row("hidden-define", () -> {
            Class<?> h = lookup.defineHiddenClass(def("L5W44Hid1"), false).lookupClass();
            return h.getName().replaceAll("0x[0-9a-f]+", "0xH") + " " + h.isHidden() + " " + (h.getClassLoader() == APP)
                    + " " + (h.getModule() == L5W44LoaderReview.class.getModule()) + " " + (h.getPackage() == L5W44LoaderReview.class.getPackage())
                    + " " + attempt(() -> Class.forName(h.getName(), false, APP));
        });
        row("hidden-define-other-package", () -> attempt(() -> lookup.defineHiddenClass(def("x44/Hid"), false)));
        row("hidden-define-no-package-access", () -> attempt(() -> lookup.dropLookupMode(Lookup.PACKAGE).defineHiddenClass(def("L5W44Hid2"), false)));
        row("hidden-define-twice", () -> {
            byte[] b = def("L5W44Hid3");
            Class<?> h1 = lookup.defineHiddenClass(b, false).lookupClass();
            Class<?> h2 = lookup.defineHiddenClass(b, false).lookupClass();
            return (h1 != h2) + " " + h1.getName().startsWith("L5W44Hid3/0x") + " " + h2.getName().startsWith("L5W44Hid3/0x");
        });
        row("hidden-nestmate", () -> {
            Class<?> h = lookup.defineHiddenClass(def("L5W44Hid4"), false, Lookup.ClassOption.NESTMATE).lookupClass();
            return (h.getNestHost() == L5W44LoaderReview.class) + " " + h.isNestmateOf(L5W44LoaderReview.class);
        });
    }

    public static class Def {
        public Def() {}
    }

    static class InitA { static { INITS.add("A"); } }
    static class InitB { static { INITS.add("B"); } }
    static class InitC { static { INITS.add("C"); } }
    static class InitD { static { INITS.add("D"); } }
    static class InitE { static { INITS.add("E"); } }
    static class Boom { static { if (true) throw new RuntimeException("boom"); } }
}
