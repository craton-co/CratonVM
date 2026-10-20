// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 63, lane L4: the door rows
// `CRATONVM_MH_OWNER_ID_AUTHORITY` (stage 2 of the owner-by-id proposal) moves,
// ns/op, one line per row ("bench <name> <ns/op>"), for interleaved A/B runs
// (medians of >= 5 reps; switch off vs `=1`, `--compatible`). Args: [reps].
//
//   getter-invoke          - `findGetter(Box, v)` through `invoke` at
//                            `(Object)int`: the door's instance GETTER arm,
//                            whose slot lookup is by the owner's id under
//                            the switch (by `MH_CLASS`'s name without it)
//   setter-invoke          - `findSetter(Box, v)` through `invoke` at
//                            `(Object,int)void`: the SETTER arm, likewise
//   unreflect-getter-invoke- `unreflectGetter(Box.v)` through `invoke`
//   getter-exact           - the getter through `invokeExact` at `(Box)int`:
//                            the field-accessor link (linked once; control)
//   namesake-getter-invoke - `findGetter` on a class a second loader ALSO
//                            defines: the by-name slot lookup finds the name
//                            ambiguous and falls back to the receiver's
//                            by-name field walk; the id does not
//   virtual-invoke         - `findVirtual(Box, get)` through `invoke`
//                            (control: stage 1's cast, unchanged here)
import java.io.IOException;
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W63OwnerIdBench {
    static int sink;

    public static class Box {
        public int v = 3;

        public int get() {
            return v;
        }
    }

    public static class Twin {
        public int w = 5;
    }

    static final String TWIN = "L4W63OwnerIdBench$Twin";

    /** Defines `Twin` itself (child-first); delegates the rest. */
    static final class Isolating extends ClassLoader {
        Isolating(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(TWIN)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    byte[] bytes;
                    try (InputStream in = L4W63OwnerIdBench.class.getResourceAsStream(name + ".class")) {
                        if (in == null) {
                            throw new ClassNotFoundException(name);
                        }
                        bytes = in.readAllBytes();
                    } catch (IOException e) {
                        throw new ClassNotFoundException(name, e);
                    }
                    c = defineClass(name, bytes, 0, bytes.length);
                }
                return c;
            }
        }
    }

    interface Body {
        long run(int n) throws Throwable;
    }

    static void bench(String name, int n, Body b) throws Throwable {
        b.run(Math.max(1, n / 10));
        long t0 = System.nanoTime();
        long s = b.run(n);
        long t1 = System.nanoTime();
        sink += (int) s;
        System.out.printf(java.util.Locale.ROOT, "bench %s %.1f%n", name, (t1 - t0) / (double) n);
    }

    public static void main(String[] args) throws Throwable {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle get = lk.findVirtual(Box.class, "get", MethodType.methodType(int.class));
        MethodHandle getter = lk.findGetter(Box.class, "v", int.class);
        MethodHandle setter = lk.findSetter(Box.class, "v", int.class);
        MethodHandle ugetter = lk.unreflectGetter(Box.class.getField("v"));
        ClassLoader app = L4W63OwnerIdBench.class.getClassLoader();
        Class<?> twinA = new Isolating(app).loadClass(TWIN);
        Class<?> twinB = new Isolating(app).loadClass(TWIN);
        Object twin = twinA.getConstructor().newInstance();
        Object other = twinB.getConstructor().newInstance();
        MethodHandle twinGetter = lk.findGetter(twinA, "w", int.class);
        Object box = new Box();
        Box typed = (Box) box;
        sink += other.hashCode() & 1;
        for (int rep = 0; rep < reps; rep++) {
            bench("getter-invoke", 200000, n -> {
                long s = 0;
                for (int i = 0; i < n; i++) s += (int) getter.invoke(box);
                return s;
            });
            bench("setter-invoke", 200000, n -> {
                for (int i = 0; i < n; i++) setter.invoke(box, 3);
                return n;
            });
            bench("unreflect-getter-invoke", 200000, n -> {
                long s = 0;
                for (int i = 0; i < n; i++) s += (int) ugetter.invoke(box);
                return s;
            });
            bench("getter-exact", 200000, n -> {
                long s = 0;
                for (int i = 0; i < n; i++) s += (int) getter.invokeExact(typed);
                return s;
            });
            bench("namesake-getter-invoke", 200000, n -> {
                long s = 0;
                for (int i = 0; i < n; i++) s += (int) twinGetter.invoke(twin);
                return s;
            });
            bench("virtual-invoke", 200000, n -> {
                long s = 0;
                for (int i = 0; i < n; i++) s += (int) get.invoke(box);
                return s;
            });
        }
    }
}
