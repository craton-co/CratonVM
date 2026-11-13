// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i7 wave 138, lane L5 (review of wave 137's
// `vm_util::record_in_builtin_loader`, page
// docs/feature-designs/real-jdk-mode.md):
// what the two built-in loaders' `ClassLoader.classes` lists hold when `main`
// starts, i.e. how many `ClassLoader.addClass` upcalls a Hello World boot
// made. Each entry is one upcall the VM's record made (or a Java define door
// made). HotSpot lists the main class only. A longer CratonVM list names
// the platform-module classes the VM initialized during its boot, each an
// upcall HotSpot never makes at that point; compare the lists, and time the
// boot with `CRATONVM_BUILTIN_LOADER_ADD_CLASS=0` against the default.
//
// Needs: --add-exports java.base/jdk.internal.misc=ALL-UNNAMED
//
// Run: javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d out L5W138BuiltinLoaderClassesAtBoot.java
//      F="--add-exports java.base/jdk.internal.misc=ALL-UNNAMED"
//      java $F -cp out L5W138BuiltinLoaderClassesAtBoot                     (HotSpot)
//      cratonvm --real-jdk|--compatible $F -cp out L5W138BuiltinLoaderClassesAtBoot
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags, -Xint and -Xshare:off)
// app size=1
// app L5W138BuiltinLoaderClassesAtBoot
// platform size=0
// HOTSPOT_EXPECTED_END
import java.util.ArrayList;
import java.util.List;
import jdk.internal.misc.Unsafe;

public class L5W138BuiltinLoaderClassesAtBoot {
    static List<String> names(ClassLoader loader) {
        long offset = Unsafe.getUnsafe().objectFieldOffset(ClassLoader.class, "classes");
        List<?> list = (List<?>) Unsafe.getUnsafe().getReference(loader, offset);
        List<String> out = new ArrayList<>();
        synchronized (list) {
            for (Object o : list) {
                out.add(((Class<?>) o).getName());
            }
        }
        return out;
    }

    public static void main(String[] args) {
        // Read before anything else this class touches is loaded.
        List<String> app = names(ClassLoader.getSystemClassLoader());
        List<String> platform = names(ClassLoader.getPlatformClassLoader());
        System.out.println("app size=" + app.size());
        app.stream().sorted().forEach(n -> System.out.println("app " + n));
        System.out.println("platform size=" + platform.size());
        platform.stream().sorted().forEach(n -> System.out.println("platform " + n));
    }
}
