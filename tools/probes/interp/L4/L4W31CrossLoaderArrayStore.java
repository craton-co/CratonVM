// Interpreter round i1 wave 31 — `aastore` across two loaders' copies of one
// class name.
//
// Page: docs/internal/fixed-bugs/interpreter-L4-cross-loader-type-checks-and-trace-shapes-FIXED-20261001.md
// (items 1 and 2: the last five lines are `Class.isAssignableFrom` between
// the two loaders' array classes, which compared names the same way).
//
// A child-first loader defines `L4W31CrossLoaderArrayStore$X` a second time.
// Storing the child's `X` into the application's `X[]` is an
// `ArrayStoreException` on HotSpot, and so is storing the child's `X[]` into
// the application's `X[][]`. Before wave 31, CratonVM `--jdk-only` walked the
// value's supertypes BY NAME and admitted both. The legal stores (each loader's
// value into its own array, anything into `Object[]`) must stay legal.
//
// `--compatible` keeps its by-name rule: the three cross-loader stores are
// admitted and the cross-loader `isAssignableFrom` lines print `true`; only the
// default (`--jdk-only`) run is compared with HotSpot. `Object[] from child
// X[]` is `true` in every mode (it was `false` before wave 31: the component
// name is ambiguous across the two loaders).
//
// Run (no setup):
//   javac -d out L4W31CrossLoaderArrayStore.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L4W31CrossLoaderArrayStore

import java.io.InputStream;
import java.lang.reflect.Array;

public class L4W31CrossLoaderArrayStore {
    public static class X {}

    static final class ChildFirst extends ClassLoader {
        ChildFirst(ClassLoader parent) {
            super("child", parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(X.class.getName())) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    try (InputStream in = getParent().getResourceAsStream(name.replace('.', '/') + ".class")) {
                        byte[] b = in.readAllBytes();
                        c = defineClass(name, b, 0, b.length);
                    } catch (java.io.IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return c;
            }
        }
    }

    static void row(String label, Object[] array, Object value) {
        try {
            array[0] = value;
            System.out.println(label + ": stored");
        } catch (ArrayStoreException e) {
            System.out.println(label + ": " + e);
        }
    }

    public static void main(String[] args) throws Exception {
        Class<?> childX = new ChildFirst(L4W31CrossLoaderArrayStore.class.getClassLoader()).loadClass(X.class.getName());
        System.out.println("distinct: " + (childX != X.class));
        Object childValue = childX.getDeclaredConstructor().newInstance();
        Object[] childArray = (Object[]) Array.newInstance(childX, 1);

        row("app X[] <- child X", new X[1], childValue);
        row("app X[][] <- child X[]", new X[1][], childArray);
        row("child X[] <- child X", childArray, childValue);
        row("child X[] <- app X", childArray, new X());
        row("app X[] <- app X", new X[1], new X());
        row("Object[] <- child X", new Object[1], childValue);
        row("Object[][] <- child X[]", new Object[1][], childArray);

        // Item 2: `Class.isAssignableFrom` between the two loaders' array classes.
        Class<?> childArrayClass = childArray.getClass();
        System.out.println("X[] from child X[]: " + X[].class.isAssignableFrom(childArrayClass));
        System.out.println("child X[] from X[]: " + childArrayClass.isAssignableFrom(X[].class));
        System.out.println("X[][] from child X[][]: "
                + X[][].class.isAssignableFrom(Array.newInstance(childArrayClass, 0).getClass()));
        System.out.println("Object[] from child X[]: " + Object[].class.isAssignableFrom(childArrayClass));
        System.out.println("child X[] from child X[]: " + childArrayClass.isAssignableFrom(childArrayClass));
    }
}
