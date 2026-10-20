// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L5: the array class over a class a USER
// loader defined is one class, whichever door reaches it (JVMS 5.3.3), in
// both modes.
//
// Under `--compatible`, `Object.getClass()` of such an array minted a fresh,
// unregistered `Class` object on every call (`native_object_get_class`, the
// `loader_id_of_class >= 3` arm), so `a.getClass() != a.getClass()`, and the
// class was in neither `getAllLoadedClasses` nor the loader's
// `getInitiatedClasses` (`L3W42InitiatedArrays` `gen[]=false`). `--jdk-only`
// answered the class manager's per-loader array class
// (`lang_class::strict_array_class_id`), which both modes now do for a user
// loader's component.
//
// Rows (each `true`/`false`):
//   same      -- a.getClass() == a.getClass();
//   twice     -- two arrays' classes are the same Class;
//   arraytype -- gen.arrayType() == a.getClass();
//   component -- a.getClass().getComponentType() == gen;
//   loader    -- a.getClass().getClassLoader() == the defining loader;
//   dim2      -- Array.newInstance(gen, 0, 0).getClass().getComponentType() == a.getClass();
//   forname   -- Class.forName("[L...Gen;", false, loader) == a.getClass();
//   name      -- the array class's name.
//
// HotSpot 25 prints (measured, JDK 25.0.3; the same with -Xint):
//     same=true twice=true arraytype=true component=true loader=true dim2=true forname=true
//     name=[LL5W48UserArrayClassIdentity$Gen;
// CratonVM `--compatible` before wave 48 (from the code): `same=false
// twice=false arraytype=false dim2=false forname=false`.
import java.io.InputStream;
import java.lang.reflect.Array;

public class L5W48UserArrayClassIdentity {
    public static class Gen {
    }

    static final class Own extends ClassLoader {
        Own() {
            super(L5W48UserArrayClassIdentity.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (name.endsWith("$Gen")) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        c = defineGen(name);
                    }
                    return c;
                }
                return super.loadClass(name, resolve);
            }
        }

        private Class<?> defineGen(String name) throws ClassNotFoundException {
            try (InputStream in = L5W48UserArrayClassIdentity.class
                    .getResourceAsStream("L5W48UserArrayClassIdentity$Gen.class")) {
                byte[] b = in.readAllBytes();
                return defineClass(name, b, 0, b.length);
            } catch (java.io.IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Own own = new Own();
        String genName = L5W48UserArrayClassIdentity.class.getName() + "$Gen";
        Class<?> gen = own.loadClass(genName);
        Object a = Array.newInstance(gen, 1);
        Object b = Array.newInstance(gen, 2);
        Class<?> ac = a.getClass();
        Object a2 = Array.newInstance(gen, 0, 0);
        Class<?> byName;
        try {
            byName = Class.forName("[L" + genName + ";", false, own);
        } catch (Throwable t) {
            byName = null;
        }
        System.out.println("same=" + (a.getClass() == a.getClass())
                + " twice=" + (ac == b.getClass())
                + " arraytype=" + (gen.arrayType() == ac)
                + " component=" + (ac.getComponentType() == gen)
                + " loader=" + (ac.getClassLoader() == own)
                + " dim2=" + (a2.getClass().getComponentType() == ac)
                + " forname=" + (byName == ac));
        System.out.println("name=" + ac.getName());
    }
}
