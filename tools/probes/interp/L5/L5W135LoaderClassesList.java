// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i7 wave 135, lane L5
// (`docs/feature-designs/real-jdk-mode.md`):
// what each define door leaves in the defining loader's private
// `ClassLoader.classes` list. HotSpot's `define_instance_class` calls
// `ClassLoader.addClass` exactly, for every non-null loader:
//   * a user loader that declares its own `addClass(Class)` still gets the
//     class in `ClassLoader.classes` (its own method is never called);
//   * `Unsafe.defineClass0` into the APPLICATION loader records the class
//     there too (CratonVM's door pinned only a user or the platform loader).
// `CRATONVM_DEFINE_ADD_CLASS_EXACT=0` is the negative control (`shadow` and
// `unsafe-app` rows print `listed=false`).
//
// Needs: --add-exports java.base/jdk.internal.misc=ALL-UNNAMED
//
// Run: javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d out L5W135LoaderClassesList.java
//      F="--add-exports java.base/jdk.internal.misc=ALL-UNNAMED"
//      java $F -cp out L5W135LoaderClassesList                         (HotSpot reference)
//      cratonvm --real-jdk $F -cp out L5W135LoaderClassesList
//      cratonvm --compatible $F -cp out L5W135LoaderClassesList
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// plain defineClass listed=true own-calls=0
// shadow defineClass listed=true own-calls=0
// unsafe-user listed=true
// unsafe-app listed=true
// app-own-class listed=true
// HOTSPOT_EXPECTED_END

import java.io.InputStream;
import java.util.List;
import jdk.internal.misc.Unsafe;

public class L5W135LoaderClassesList {
    public static class PayloadA {}

    public static class PayloadB {}

    public static class PayloadC {}

    public static class PayloadD {}

    static byte[] bytesOf(String name) throws Exception {
        try (InputStream in = L5W135LoaderClassesList.class.getResourceAsStream("/" + name + ".class")) {
            return in.readAllBytes();
        }
    }

    // `ClassLoader.classes` is filtered from reflection; its offset by name.
    static final long CLASSES = Unsafe.getUnsafe().objectFieldOffset(ClassLoader.class, "classes");

    static boolean listed(ClassLoader loader, Class<?> c) throws Exception {
        List<?> list = (List<?>) Unsafe.getUnsafe().getReference(loader, CLASSES);
        synchronized (list) {
            for (Object o : list) {
                if (o == c) {
                    return true;
                }
            }
        }
        return false;
    }

    static class Plain extends ClassLoader {
        Plain() {
            super(L5W135LoaderClassesList.class.getClassLoader());
        }

        Class<?> def(String n, byte[] b) {
            return defineClass(n, b, 0, b.length);
        }
    }

    static class Shadow extends ClassLoader {
        int calls;

        Shadow() {
            super(L5W135LoaderClassesList.class.getClassLoader());
        }

        public void addClass(Class<?> c) {
            calls++;
        }

        Class<?> def(String n, byte[] b) {
            return defineClass(n, b, 0, b.length);
        }
    }

    public static void main(String[] args) throws Exception {
        String a = "L5W135LoaderClassesList$PayloadA";
        Plain plain = new Plain();
        Class<?> ca = plain.def(a, bytesOf(a));
        System.out.println("plain defineClass listed=" + listed(plain, ca) + " own-calls=0");

        Shadow shadow = new Shadow();
        Class<?> cb = shadow.def(a, bytesOf(a));
        System.out.println("shadow defineClass listed=" + listed(shadow, cb) + " own-calls=" + shadow.calls);

        Unsafe u = Unsafe.getUnsafe();
        String b = "L5W135LoaderClassesList$PayloadB";
        Plain other = new Plain();
        byte[] bb = bytesOf(b);
        Class<?> cc = u.defineClass(b, bb, 0, bb.length, other, null);
        System.out.println("unsafe-user listed=" + listed(other, cc));

        // A class the application loader has not loaded yet: defined into it
        // directly through Unsafe (PayloadC is never referenced otherwise).
        String c = "L5W135LoaderClassesList$PayloadC";
        ClassLoader app = ClassLoader.getSystemClassLoader();
        byte[] bc = bytesOf(c);
        Class<?> cd = u.defineClass(c, bc, 0, bc.length, app, null);
        System.out.println("unsafe-app listed=" + (cd.getClassLoader() == app && listed(app, cd)));

        // Control: the application loader's own load of a class.
        Class<?> ce = Class.forName("L5W135LoaderClassesList$PayloadD", false, app);
        System.out.println("app-own-class listed=" + listed(app, ce));
    }
}
