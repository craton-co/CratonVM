import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

/*
 * Interpreter round i1, wave 16, lane L2: JVMS 6.5 `new` of an abstract class
 * throws `InstantiationError`, after resolution and BEFORE the class is
 * initialized, on every execution (interpreted and compiled).
 *
 * javac refuses `new` of an abstract class, so the probe makes one itself:
 * it reads the compiled `NaieTarget.class` (a concrete class in this file),
 * sets ACC_ABSTRACT in its access flags, and defines it, together with
 * `NaieMaker` (which does `new NaieTarget()`), in a child-first loader. No
 * extra setup: the class files this source compiles to are all it needs.
 *
 * Before wave 16 CratonVM allocated an instance of the abstract class, ran
 * its `<clinit>` and constructor, and printed `make: NaieTarget@...`-style
 * lines instead.
 *
 * HotSpot 25 prints exactly (and CratonVM must, with and without --nojit):
 *
 *   make: java.lang.InstantiationError: NaieTarget
 *   make again: java.lang.InstantiationError: NaieTarget
 *   loop caught=30000
 *   NaieTarget.<clinit>
 *   ping=42
 */
public class NewAbstractInstantiationError {
    static final class PatchingLoader extends ClassLoader {
        PatchingLoader(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals("NaieTarget") && !name.equals("NaieMaker")) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    byte[] b = read(name);
                    if (name.equals("NaieTarget")) {
                        addAccessFlags(b, 0x0400); // ACC_ABSTRACT
                    }
                    c = defineClass(name, b, 0, b.length);
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
        }

        private static byte[] read(String name) throws ClassNotFoundException {
            try (InputStream in =
                    NewAbstractInstantiationError.class.getResourceAsStream("/" + name + ".class")) {
                if (in == null) {
                    throw new ClassNotFoundException(name);
                }
                return in.readAllBytes();
            } catch (java.io.IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }
    }

    private static int u2(byte[] b, int p) {
        return ((b[p] & 0xff) << 8) | (b[p + 1] & 0xff);
    }

    /** OR `flags` into the class file's `access_flags` (after the constant pool). */
    static void addAccessFlags(byte[] b, int flags) {
        int count = u2(b, 8);
        int p = 10;
        for (int i = 1; i < count; i++) {
            int tag = b[p] & 0xff;
            switch (tag) {
                case 1: // Utf8
                    p += 3 + u2(b, p + 1);
                    break;
                case 3: case 4: // Integer, Float
                case 9: case 10: case 11: case 12: // refs, NameAndType
                case 17: case 18: // Dynamic, InvokeDynamic
                    p += 5;
                    break;
                case 5: case 6: // Long, Double take two entries
                    p += 9;
                    i++;
                    break;
                case 7: case 8: case 16: case 19: case 20: // Class, String, MethodType, Module, Package
                    p += 3;
                    break;
                case 15: // MethodHandle
                    p += 4;
                    break;
                default:
                    throw new IllegalStateException("constant pool tag " + tag);
            }
        }
        int access = u2(b, p) | flags;
        b[p] = (byte) (access >> 8);
        b[p + 1] = (byte) access;
    }

    static String call(Method m) throws Exception {
        try {
            return String.valueOf(m.invoke(null));
        } catch (InvocationTargetException e) {
            return String.valueOf(e.getCause());
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader loader = new PatchingLoader(NewAbstractInstantiationError.class.getClassLoader());
        Class<?> maker = Class.forName("NaieMaker", true, loader);
        Method make = maker.getDeclaredMethod("make");
        make.setAccessible(true);
        Method tryMany = maker.getDeclaredMethod("tryMany", int.class);
        tryMany.setAccessible(true);
        Method ping = maker.getDeclaredMethod("ping");
        ping.setAccessible(true);

        System.out.println("make: " + call(make));
        System.out.println("make again: " + call(make));
        // Long enough for the loop to be compiled: the compiled `new` must
        // throw too, and must not initialize the class either.
        System.out.println("loop caught=" + tryMany.invoke(null, 30000));
        // The first thing that initializes the abstract class.
        System.out.println("ping=" + call(ping));
    }
}

class NaieTarget {
    static {
        System.out.println("NaieTarget.<clinit>");
    }

    NaieTarget() {
    }

    static int ping() {
        return 42;
    }
}

class NaieMaker {
    static Object make() {
        return new NaieTarget();
    }

    static int tryMany(int n) {
        int caught = 0;
        for (int i = 0; i < n; i++) {
            try {
                Object o = new NaieTarget();
                if (o == null) {
                    caught--;
                }
            } catch (InstantiationError e) {
                caught++;
            }
        }
        return caught;
    }

    static int ping() {
        return NaieTarget.ping();
    }
}
