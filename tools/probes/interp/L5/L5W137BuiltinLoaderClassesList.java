// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i7 wave 137, lane L5
// (`docs/feature-designs/real-jdk-mode.md`):
// HotSpot's `SystemDictionary::define_instance_class` calls
// `ClassLoader.addClass` for EVERY class a non-boot loader defines, so the
// application loader's private `ClassLoader.classes` list holds every
// class-path class it loaded, once, supertypes before the classes that
// extend them. CratonVM loads class-path classes from its own class path
// (the JDK's `BuiltinClassLoader.defineClass` never runs), and the list held
// none of them. Rows, each through a different door:
//   forname-noinit  Class.forName(n, false, app)  (never linked)
//   loadclass       app.loadClass(n)              (never linked)
//   new             `new NewTarget()`             (resolution + initialization)
//   array-element   `ElemTarget[].class`          (ldc of an array class; the element is loaded only)
//   order           Sub extends Base implements Iface: Iface, then Base, then Sub
//                   (HotSpot's parser resolves -- defines -- the interfaces
//                   before the superclass)
//   lambda          a lambda's hidden class is never listed
//   once            no class is listed twice
//   platform        Class.forName("java.sql.Date", false, platform): the
//                   PLATFORM loader's list (CratonVM tags image classes with
//                   the boot loader's id; HotSpot's platform loader defines
//                   them, CDS on and off)
//   boot            java.util.logging.Logger (a boot module other than
//                   java.base) is in neither built-in loader's list
// `CRATONVM_BUILTIN_LOADER_ADD_CLASS=0` is the negative control (every
// `listed=` row but `lambda` prints false, `order` prints `missing`).
//
// Needs: --add-exports java.base/jdk.internal.misc=ALL-UNNAMED
//
// Run: javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d out L5W137BuiltinLoaderClassesList.java
//      F="--add-exports java.base/jdk.internal.misc=ALL-UNNAMED"
//      java $F -cp out L5W137BuiltinLoaderClassesList                         (HotSpot reference)
//      cratonvm --real-jdk $F -cp out L5W137BuiltinLoaderClassesList
//      cratonvm --compatible $F -cp out L5W137BuiltinLoaderClassesList
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// main listed=true
// forname-noinit listed=true
// loadclass listed=true
// new listed=true
// array-element listed=true
// order Iface<Base<Sub
// lambda listed=false
// once duplicates=0
// platform listed=true
// boot listed=false
// HOTSPOT_EXPECTED_END

import java.util.ArrayList;
import java.util.IdentityHashMap;
import java.util.List;
import java.util.Map;
import jdk.internal.misc.Unsafe;

public class L5W137BuiltinLoaderClassesList {
    public static class ForNameTarget {}

    public static class LoadClassTarget {}

    public static class NewTarget {}

    public static class ElemTarget {}

    public static class Base {}

    public interface Iface {}

    public static class Sub extends Base implements Iface {}

    // `ClassLoader.classes` is filtered from reflection; its offset by name.
    static final long CLASSES = Unsafe.getUnsafe().objectFieldOffset(ClassLoader.class, "classes");

    static List<Object> snapshot(ClassLoader loader) {
        List<?> list = (List<?>) Unsafe.getUnsafe().getReference(loader, CLASSES);
        synchronized (list) {
            return new ArrayList<>(list);
        }
    }

    static int indexOf(List<Object> list, Class<?> c) {
        for (int i = 0; i < list.size(); i++) {
            if (list.get(i) == c) {
                return i;
            }
        }
        return -1;
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        String outer = L5W137BuiltinLoaderClassesList.class.getName();

        Class<?> forName = Class.forName(outer + "$ForNameTarget", false, app);
        Class<?> loadClass = app.loadClass(outer + "$LoadClassTarget");
        Object made = new NewTarget();
        Class<?> elem = ElemTarget[].class.getComponentType();
        Object sub = new Sub();
        Runnable lambda = () -> {};

        List<Object> list = snapshot(app);
        System.out.println("main listed=" + (indexOf(list, L5W137BuiltinLoaderClassesList.class) >= 0));
        System.out.println("forname-noinit listed=" + (indexOf(list, forName) >= 0));
        System.out.println("loadclass listed=" + (indexOf(list, loadClass) >= 0));
        System.out.println("new listed=" + (indexOf(list, made.getClass()) >= 0));
        System.out.println("array-element listed=" + (indexOf(list, elem) >= 0));

        int b = indexOf(list, Base.class);
        int i = indexOf(list, Iface.class);
        int s = indexOf(list, sub.getClass());
        String order;
        if (b < 0 || i < 0 || s < 0) {
            order = "missing";
        } else {
            // The three names, by their position in the list.
            String[] names = new String[3];
            int[][] at = {{b, 0}, {i, 1}, {s, 2}};
            java.util.Arrays.sort(at, (x, y) -> Integer.compare(x[0], y[0]));
            String[] label = {"Base", "Iface", "Sub"};
            for (int k = 0; k < 3; k++) {
                names[k] = label[at[k][1]];
            }
            order = String.join("<", names);
        }
        System.out.println("order " + order);
        System.out.println("lambda listed=" + (indexOf(list, lambda.getClass()) >= 0));

        Map<Object, Integer> seen = new IdentityHashMap<>();
        int duplicates = 0;
        for (Object o : list) {
            if (seen.merge(o, 1, Integer::sum) == 2) {
                duplicates++;
            }
        }
        System.out.println("once duplicates=" + duplicates);

        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        Class<?> date = Class.forName("java.sql.Date", false, platform);
        System.out.println("platform listed=" + (date.getClassLoader() == platform
                && indexOf(snapshot(platform), date) >= 0));
        Class<?> logger = Class.forName("java.util.logging.Logger", false, null);
        System.out.println("boot listed=" + (indexOf(snapshot(platform), logger) >= 0
                || indexOf(snapshot(app), logger) >= 0));
    }
}
