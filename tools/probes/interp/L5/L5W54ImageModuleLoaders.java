// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 54, lane L5 -- which loader owns a runtime-image
// module that is not a boot module, asked three ways:
//
//   boot-*    `Class.forName(name, false, null)`: the BOOTSTRAP loader finds
//             only boot-module classes. CratonVM's `forName0` native admitted
//             any `java/` / `com/sun/` name by prefix, so `java.sql.Date`
//             (platform module) and `com.sun.tools.javac.Main` (application
//             module) were found (both modes).
//   find-*    `ModuleLayer.boot().findLoader(name)` and `Module.getClassLoader()`
//             agree with `Class.getClassLoader()` of a class in the module.
//             Under `--compatible` the `jdk.compiler` module answered `null`
//             while its classes answered the application loader.
//
// Rows first on a cold VM, again last.
//
// Run (no setup):
//   javac -d out L5W54ImageModuleLoaders.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W54ImageModuleLoaders
//
// Measured on a local dev build before the fix (wave-53 head plus this lane's
// other changes, Windows): `boot-sql-N=class java.sql.Date` (both modes),
// `boot-javac-N=class com.sun.tools.javac.Main` (both modes), and under
// `--compatible` `find-compiler-N=false module-compiler=false`.
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   boot-sql-1=java.lang.ClassNotFoundException: java/sql/Date
//   boot-javac-1=java.lang.ClassNotFoundException: com/sun/tools/javac/Main
//   boot-string-1=class java.lang.String
//   boot-sql-again-1=java.lang.ClassNotFoundException: java/sql/Date
//   find-sql-1=true module-sql=true class-sql=true
//   find-compiler-1=true module-compiler=true class-compiler=true
//   find-base-1=null module-base=null
//   boot-sql-2=java.lang.ClassNotFoundException: java/sql/Date
//   boot-javac-2=java.lang.ClassNotFoundException: com/sun/tools/javac/Main
//   boot-string-2=class java.lang.String
//   boot-sql-again-2=java.lang.ClassNotFoundException: java/sql/Date
//   find-sql-2=true module-sql=true class-sql=true
//   find-compiler-2=true module-compiler=true class-compiler=true
//   find-base-2=null module-base=null

public class L5W54ImageModuleLoaders {
    static void boot(String label, String name) {
        try {
            System.out.println(label + "=" + Class.forName(name, false, null));
        } catch (Throwable t) {
            System.out.println(label + "=" + t);
        }
    }

    static void round(int n) throws Exception {
        boot("boot-sql-" + n, "java.sql.Date");
        boot("boot-javac-" + n, "com.sun.tools.javac.Main");
        boot("boot-string-" + n, "java.lang.String");
        // After the platform loader has loaded it.
        Class.forName("java.sql.Date");
        boot("boot-sql-again-" + n, "java.sql.Date");
        ClassLoader app = ClassLoader.getSystemClassLoader();
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        ModuleLayer layer = ModuleLayer.boot();
        System.out.println("find-sql-" + n + "=" + (layer.findLoader("java.sql") == platform)
                + " module-sql=" + (layer.findModule("java.sql").orElseThrow().getClassLoader() == platform)
                + " class-sql=" + (Class.forName("java.sql.Date").getClassLoader() == platform));
        Class<?> javac = Class.forName("com.sun.tools.javac.Main");
        System.out.println("find-compiler-" + n + "=" + (layer.findLoader("jdk.compiler") == app)
                + " module-compiler=" + (javac.getModule().getClassLoader() == app)
                + " class-compiler=" + (javac.getClassLoader() == app));
        System.out.println("find-base-" + n + "=" + layer.findLoader("java.base")
                + " module-base=" + Object.class.getModule().getClassLoader());
    }

    public static void main(String[] args) throws Exception {
        round(1);
        round(2);
    }
}
