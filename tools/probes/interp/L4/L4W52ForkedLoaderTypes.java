// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 52, lane L4: the forked-classloader shape the
// `--compatible` by-name type-check rule was added for (Spring Boot's
// `ModifiedClassPathClassLoader` / `@ForkedClassPath`, Spring's
// `@CompileWithForkedClassLoader`): a child-first loader re-defines the test
// classes from the application class path and runs a body INSIDE the fork,
// whose casts, `instanceof`s, array stores, `Class.isInstance` and lambda
// results all name the fork's own copies. HotSpot admits every one of them
// (each check is against the class the fork itself resolved). The rows are
// what the alignment rows `namespaced-class-types` / `namespaced-array-stores`
// (`types/src/hotspot_alignment.rs`) must keep admitting under `--compatible`
// once they are on; the `cross-*` rows are the app copy against the fork's,
// which HotSpot refuses. Page
// `docs/internal/fixed-bugs/interpreter-L4-compatible-type-checks-keep-the-by-name-loader-rule-FIXED-20261016.md`.
// The first fork row is repeated last; the `hot` row runs the fork's checks
// until they are compiled.
//
// Run: javac -d out L4W52ForkedLoaderTypes.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W52ForkedLoaderTypes
//
// Expected HotSpot 25 output (25.0.3, default flags):
//   fork-loader: fork
//   fork-cast: ok Model
//   fork-instanceof-iface: true
//   fork-array-store: ok 1
//   fork-array-cast: ok
//   fork-is-instance: true
//   fork-lambda-result: ok Model
//   fork-collection-cast: ok Model
//   fork-stream-map: ok [Model]
//   fork-enum-switch: B
//   fork-record-pattern: Pair[a=Model, b=1]
//   fork-annotation-enum-array: true
//   fork-annotation-class: true
//   fork-proxy-cast: Proxy
//   fork-forname-cast: ok Model
//   fork-enum-valueof: true
//   fork-hot: 30000
//   fork-cast-again: ok Model
//   cross-is-instance: false
//   cross-cast: ClassCastException
//   cross-array-store: ArrayStoreException
//   cross-same-name: true
import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.Array;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;
import java.util.stream.Collectors;

public class L4W52ForkedLoaderTypes {
    public interface Iface {
        String name();
    }

    public static class Model implements Iface {
        public String name() {
            return "Model";
        }
    }

    public enum Kind { A, B }

    public record Pair(Object a, int b) {
    }

    @java.lang.annotation.Retention(java.lang.annotation.RetentionPolicy.RUNTIME)
    public @interface Ann {
        Kind[] kinds();

        Class<?> type();

        Kind one();
    }

    @Ann(kinds = {Kind.A, Kind.B}, type = Model.class, one = Kind.B)
    public static class Annotated {
    }

    /** Runs inside the fork: every class it names is the fork's copy. */
    public static class Body implements Supplier<List<String>> {
        static Object make() {
            return new Model();
        }

        static String row(String name, Object value) {
            return name + ": " + value;
        }

        public List<String> get() {
            List<String> out = new ArrayList<>();
            out.add(row("fork-loader", getClass().getClassLoader() instanceof Fork ? "fork" : "other"));
            Object o = make();
            out.add(row("fork-cast", "ok " + ((Model) o).name()));
            out.add(row("fork-instanceof-iface", o instanceof Iface));
            Object[] arr = new Model[1];
            arr[0] = o;
            out.add(row("fork-array-store", "ok " + arr.length));
            Object a2 = arr;
            out.add(row("fork-array-cast", ((Model[]) a2).length == 1 ? "ok" : "bad"));
            out.add(row("fork-is-instance", Model.class.isInstance(o)));
            Supplier<Object> s = Body::make;
            out.add(row("fork-lambda-result", "ok " + ((Model) s.get()).name()));
            List<Object> list = new ArrayList<>();
            list.add(o);
            out.add(row("fork-collection-cast", "ok " + ((Model) list.get(0)).name()));
            out.add(row("fork-stream-map", "ok " + list.stream().map(x -> ((Iface) x).name()).collect(Collectors.toList())));
            Kind k = Kind.valueOf("B");
            String sw = switch (k) {
                case A -> "A";
                case B -> "B";
            };
            out.add(row("fork-enum-switch", sw));
            Object p = new Pair(o, 1);
            String rp = p instanceof Pair(Object x, int y) ? "Pair[a=" + ((Iface) x).name() + ", b=" + y + "]" : "no";
            out.add(row("fork-record-pattern", rp));
            Ann ann = Annotated.class.getAnnotation(Ann.class);
            Object kinds = ann.kinds();
            out.add(row("fork-annotation-enum-array", ((Kind[]) kinds)[1] == Kind.B && ann.one() == Kind.B));
            out.add(row("fork-annotation-class", ann.type() == Model.class));
            Object proxy = java.lang.reflect.Proxy.newProxyInstance(getClass().getClassLoader(),
                new Class<?>[] {Iface.class}, (pr, m, as) -> "Proxy");
            out.add(row("fork-proxy-cast", ((Iface) proxy).name()));
            try {
                Object byName = Class.forName("L4W52ForkedLoaderTypes$Model", true, getClass().getClassLoader())
                    .getDeclaredConstructor().newInstance();
                out.add(row("fork-forname-cast", "ok " + ((Model) byName).name()));
            } catch (ReflectiveOperationException e) {
                out.add(row("fork-forname-cast", e));
            }
            Object en = Enum.valueOf(Kind.class, "A");
            out.add(row("fork-enum-valueof", en == Kind.A));
            int ok = 0;
            for (int i = 0; i < 30000; i++) {
                Object m = (i & 1) == 0 ? o : make();
                if (m instanceof Iface && ((Model) m).name().length() == 5) {
                    Object[] h = new Model[1];
                    h[0] = m;
                    ok++;
                }
            }
            out.add(row("fork-hot", ok));
            out.add(row("fork-cast-again", "ok " + ((Model) make()).name()));
            return out;
        }
    }

    /** Child-first for this probe's own classes, parent-first for the rest. */
    public static class Fork extends ClassLoader {
        Fork(ClassLoader parent) {
            super("fork", parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (!name.startsWith("L4W52ForkedLoaderTypes$") || name.equals("L4W52ForkedLoaderTypes$Fork")) {
                    return super.loadClass(name, resolve);
                }
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    String res = name.replace('.', '/') + ".class";
                    try (InputStream in = getParent().getResourceAsStream(res)) {
                        byte[] b = in.readAllBytes();
                        c = defineClass(name, b, 0, b.length);
                    } catch (IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
        }
    }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Throwable {
        Fork fork = new Fork(L4W52ForkedLoaderTypes.class.getClassLoader());
        Class<?> body = fork.loadClass("L4W52ForkedLoaderTypes$Body");
        Supplier<List<String>> run = (Supplier<List<String>>) body.getDeclaredConstructor().newInstance();
        for (String line : run.get()) {
            System.out.println(line);
        }
        Class<?> forkModel = fork.loadClass("L4W52ForkedLoaderTypes$Model");
        Object forkObj = forkModel.getDeclaredConstructor().newInstance();
        System.out.println("cross-is-instance: " + Model.class.isInstance(forkObj));
        String cast;
        try {
            Model m = (Model) forkObj;
            cast = "cast ok " + m;
        } catch (ClassCastException e) {
            cast = "ClassCastException";
        }
        System.out.println("cross-cast: " + cast);
        String store;
        try {
            Object[] appArr = new Model[1];
            appArr[0] = forkObj;
            store = "stored";
        } catch (ArrayStoreException e) {
            store = "ArrayStoreException";
        }
        System.out.println("cross-array-store: " + store);
        System.out.println("cross-same-name: " + forkModel.getName().equals(Model.class.getName()));
    }
}
