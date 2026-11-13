// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i7 wave 138, lane L5 (page
// docs/feature-designs/real-jdk-mode.md):
// the boot loader's search (`BootLoader.loadClassOrNull`, which reaches
// `JavaLangAccess.findBootstrapClassOrNull` and `ClassLoader.findBootstrapClass`)
// answers only classes of BOOT modules. An application class-path class and a
// PLATFORM module's class (`java.sql.Date`) are `null` there on HotSpot, loaded
// or not; a boot module's class (`java.util.logging.Logger`) is found.
//
// Needs: --add-exports java.base/jdk.internal.loader=ALL-UNNAMED
//
// Run: javac --add-exports java.base/jdk.internal.loader=ALL-UNNAMED -d out L5W138BootSearchNonBootNames.java
//      F="--add-exports java.base/jdk.internal.loader=ALL-UNNAMED"
//      java $F -cp out L5W138BootSearchNonBootNames                       (HotSpot)
//      cratonvm --real-jdk|--compatible [--nojit] $F -cp out L5W138BootSearchNonBootNames
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// app-loaded null
// app-unloaded null
// platform-unloaded null
// platform-loaded null
// boot-module java.util.logging.Logger loader=null
// HOTSPOT_EXPECTED_END
import jdk.internal.loader.BootLoader;

public class L5W138BootSearchNonBootNames {
    public static class Loaded {}

    static String boot(String name) {
        try {
            Class<?> c = BootLoader.loadClassOrNull(name);
            return c == null ? "null" : c.getName() + " loader=" + c.getClassLoader();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        new Loaded();
        System.out.println("app-loaded " + boot(L5W138BootSearchNonBootNames.class.getName() + "$Loaded"));
        System.out.println("app-unloaded " + boot(L5W138BootSearchNonBootNames.class.getName() + "$Never"));
        System.out.println("platform-unloaded " + boot("java.sql.Time"));
        Class.forName("java.sql.Date", true, ClassLoader.getPlatformClassLoader());
        System.out.println("platform-loaded " + boot("java.sql.Date"));
        System.out.println("boot-module " + boot("java.util.logging.Logger"));
    }

    static class Never {}
}
