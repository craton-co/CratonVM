// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;
import java.lang.reflect.RecordComponent;
import java.lang.reflect.Type;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Callable;

/**
 * SBR round 3, lane {@code ref}: {@code Class.getPackage / getTypeName}, {@code ClassLoader.loadClass},
 * {@code Field.getRoot} (through {@code Field.copy}) and {@code RecordComponent.getGenericType}.
 * Diff against HotSpot.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . Sbr3RefmiscClass
 * </pre>
 */
public final class Sbr3RefmiscClass {
    record R<T>(int a, List<String> names, Map<String, T> map, T[] arr, int[] ints, String plain) {}

    record Plain(int x, long y) {}

    public static int pubField = 3;
    public String inst = "i";

    static void row(String name, Callable<Object> s) {
        try {
            System.out.println(name + " = " + s.call());
        } catch (Throwable t) {
            System.out.println(name + " threw " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = Sbr3RefmiscClass.class.getClassLoader();
        row("pkg String", () -> String.class.getPackage() + "|" + String.class.getPackage().getName());
        row("pkg unnamed", () -> Sbr3RefmiscClass.class.getPackage() + "|" + Sbr3RefmiscClass.class.getPackage().getName()
                + "|" + Sbr3RefmiscClass.class.getPackageName().isEmpty());
        row("pkg array", () -> String[].class.getPackage() + "|" + int[].class.getPackage());
        row("pkg prim", () -> int.class.getPackage() + "|" + void.class.getPackage());
        row("pkg nested", () -> Map.Entry.class.getPackage().getName());
        row("pkg proxy", () -> java.lang.reflect.Proxy.newProxyInstance(app, new Class<?>[] {Runnable.class}, (p, m, a) -> null)
                .getClass().getPackage().getName().isEmpty());
        row("pkg lambda", () -> ((Runnable) () -> {}).getClass().getPackage().getName().isEmpty());
        row("pkg spec", () -> {
            Package p = String.class.getPackage();
            return p.getSpecificationTitle() + "|" + p.getImplementationVersion() + "|" + p.isSealed();
        });
        row("pkg same", () -> String.class.getPackage() == Integer.class.getPackage());
        row("pkg loader", () -> java.sql.Driver.class.getPackage().getName());

        row("typeName String", () -> String.class.getTypeName());
        row("typeName arrays", () -> String[][].class.getTypeName() + "|" + int[].class.getTypeName() + "|" + Map.Entry[].class.getTypeName());
        row("typeName prim", () -> int.class.getTypeName() + "|" + void.class.getTypeName());
        row("typeName nested", () -> Map.Entry.class.getTypeName() + "|" + R.class.getTypeName());
        row("typeName anon", () -> new Object() {}.getClass().getTypeName());
        row("typeName lambda like", () -> ((Runnable) () -> {}).getClass().getTypeName().startsWith("Sbr3RefmiscClass$$Lambda"));
        row("typeName proxy", () -> java.lang.reflect.Proxy.newProxyInstance(app, new Class<?>[] {Runnable.class}, (p, m, a) -> null)
                .getClass().getTypeName().startsWith("jdk.proxy"));
        row("typeName deep array", () -> new int[0][0][0][0].getClass().getTypeName());

        row("loadClass String", () -> app.loadClass("java.lang.String").getName());
        row("loadClass user", () -> app.loadClass("Sbr3RefmiscClass").getName());
        row("loadClass nested", () -> app.loadClass("Sbr3RefmiscClass$R").getName());
        row("loadClass missing", () -> {
            try {
                return app.loadClass("no.such.Clazz");
            } catch (ClassNotFoundException e) {
                return "CNFE " + e.getMessage();
            }
        });
        row("loadClass array name", () -> {
            try {
                return app.loadClass("[Ljava.lang.String;");
            } catch (ClassNotFoundException e) {
                return "CNFE " + e.getMessage();
            }
        });
        row("loadClass slash name", () -> {
            try {
                return app.loadClass("java/lang/String");
            } catch (ClassNotFoundException e) {
                return "CNFE " + e.getMessage();
            }
        });
        row("loadClass null", () -> {
            try {
                return app.loadClass(null);
            } catch (Throwable e) {
                return e.getClass().getName();
            }
        });
        row("loadClass system", () -> ClassLoader.getSystemClassLoader().loadClass("java.util.ArrayList").getName());
        row("loadClass platform", () -> ClassLoader.getPlatformClassLoader().loadClass("java.sql.Driver").getName());
        row("loadClass platform-miss", () -> {
            try {
                return ClassLoader.getPlatformClassLoader().loadClass("Sbr3RefmiscClass");
            } catch (ClassNotFoundException e) {
                return "CNFE " + e.getMessage();
            }
        });
        row("loadClass resolve flag", () -> {
            try {
                java.lang.reflect.Method m = ClassLoader.class.getDeclaredMethod("loadClass", String.class, boolean.class);
                m.setAccessible(true);
                return ((Class<?>) m.invoke(app, "java.util.HashMap", true)).getName();
            } catch (Throwable t) {
                return t.getClass().getName() + ":" + t.getCause();
            }
        });
        row("custom loader override", () -> {
            ClassLoader cl = new ClassLoader(app) {
                @Override
                protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
                    if (name.equals("fake.Name")) {
                        return Integer.class;
                    }
                    return super.loadClass(name, resolve);
                }
            };
            try {
                return cl.loadClass("fake.Name").getName() + " " + cl.loadClass("java.lang.Long").getName();
            } catch (Throwable t) {
                return t.toString();
            }
        });
        row("loadClass is same", () -> {
            try {
                return app.loadClass("java.lang.String") == String.class;
            } catch (Throwable t) {
                return t.toString();
            }
        });

        // Field.getRoot via copy(): the root of a copy is the original
        row("field root", () -> {
            try {
                Field f = Sbr3RefmiscClass.class.getField("pubField");
                Field g = Sbr3RefmiscClass.class.getField("pubField");
                g.setAccessible(true);
                return (f.equals(g)) + " " + (f.get(null)) + " " + (g.getInt(null)) + " " + (f != g);
            } catch (Throwable t) {
                return t.toString();
            }
        });
        row("field set via copy", () -> {
            try {
                Field f = Sbr3RefmiscClass.class.getField("pubField");
                f.setInt(null, 41);
                Field g = Sbr3RefmiscClass.class.getField("pubField");
                return g.getInt(null);
            } catch (Throwable t) {
                return t.toString();
            }
        });
        row("field accessor shared", () -> {
            try {
                Sbr3RefmiscClass o = new Sbr3RefmiscClass();
                Field f = Sbr3RefmiscClass.class.getField("inst");
                f.set(o, "z");
                return Sbr3RefmiscClass.class.getField("inst").get(o);
            } catch (Throwable t) {
                return t.toString();
            }
        });
        row("field modifiers hash", () -> {
            try {
                Field f = Sbr3RefmiscClass.class.getField("pubField");
                return f.getModifiers() + " " + f.getType() + " " + f.toGenericString() + " " + (f.hashCode() == Sbr3RefmiscClass.class.getField("pubField").hashCode());
            } catch (Throwable t) {
                return t.toString();
            }
        });

        // record components
        row("record generic types", () -> {
            StringBuilder sb = new StringBuilder();
            for (RecordComponent rc : R.class.getRecordComponents()) {
                Type t = rc.getGenericType();
                sb.append(rc.getName()).append(':').append(t.getTypeName()).append(':').append(t.getClass().getSimpleName()).append(';');
            }
            return sb;
        });
        row("record plain generic types", () -> {
            StringBuilder sb = new StringBuilder();
            for (RecordComponent rc : Plain.class.getRecordComponents()) {
                sb.append(rc.getName()).append(':').append(rc.getGenericType()).append(':').append(rc.getType()).append(';');
            }
            return sb;
        });
        row("record component misc", () -> {
            RecordComponent rc = R.class.getRecordComponents()[1];
            return rc.getGenericSignature() + " " + rc + " " + rc.toString().length() + " " + rc.getAccessor().getName()
                    + " " + rc.getDeclaringRecord().getSimpleName();
        });
        row("record component cached", () -> {
            RecordComponent rc = R.class.getRecordComponents()[2];
            return rc.getGenericType() == rc.getGenericType();
        });
        System.out.println("PROBE-DONE");
    }
}
