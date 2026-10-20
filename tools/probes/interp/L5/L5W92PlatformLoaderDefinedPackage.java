// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 92, lane L5 -- the platform loader answers
// `getDefinedPackage` from its own `packages` map, as the application loader
// already does (`i55-L5-proposal-built-in-loaders-answer-packages-from-their-map`).
// HotSpot's map gets a package when a class of it is defined
// (`ClassLoader.postDefineClass` -> `getNamedPackage`; CDS's
// `define_shared_package` for an archived class), and `getDefinedPackage` is
// `packages.get(name)` then `definePackage(name, p.module())`. The VM's
// class-file probe answered a fabricated `Package` for a platform package
// with class files but no loaded class.
//
// Rows (each first, and again last):
//   sql-before-*     platform `getDefinedPackage("java.sql")` before any
//                    `java.sql` class is loaded
//   listed-before-*  is `java.sql` in platform `getDefinedPackages()`
//   (then `Class.forName("java.sql.Date", false, platform)`)
//   sql-after-*      the package's name, and its identity with
//                    `java.sql.Date.class.getPackage()`
//   listed-after-*   is `java.sql` listed now
//   javax-sql-*      `getDefinedPackage("javax.sql")` (same module, no class
//                    of it loaded)
//   nope-*           `getDefinedPackage("java.sql.nope")` (no such package)
//
// Run: javac -d out L5W92PlatformLoaderDefinedPackage.java
//      java -cp out L5W92PlatformLoaderDefinedPackage
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W92PlatformLoaderDefinedPackage
//      (`CRATONVM_PLATFORM_LOADER_PACKAGE_MAP=0` is the kill switch: the
//      `sql-before-1` / `javax-sql-*` rows then read the fabricated package.)
//
// HOTSPOT_EXPECTED (derived from the JDK 25 sources above, not measured: a
// plain `java -cp out` run loads no `java.sql` / `javax.sql` class before
// main, so both are absent from the platform loader's map until
// `java.sql.Date` is loaded; row 2 repeats row 1's "after" state):
//   sql-before-1=null
//   listed-before-1=false
//   sql-after-1=java.sql same=true
//   listed-after-1=true
//   javax-sql-1=null
//   nope-1=null
//   sql-before-2=java.sql same=true
//   listed-before-2=true
//   sql-after-2=java.sql same=true
//   listed-after-2=true
//   javax-sql-2=null
//   nope-2=null

import java.util.Arrays;
import java.util.concurrent.Callable;

public class L5W92PlatformLoaderDefinedPackage {
    static String attempt(Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    static Class<?> sqlDate;

    static String describe(ClassLoader platform, String name) {
        Package p = platform.getDefinedPackage(name);
        if (p == null) {
            return "null";
        }
        String same = sqlDate == null ? "n/a" : String.valueOf(p == sqlDate.getPackage());
        return p.getName() + " same=" + same;
    }

    static boolean listed(ClassLoader platform, String name) {
        return Arrays.stream(platform.getDefinedPackages()).anyMatch(p -> p.getName().equals(name));
    }

    public static void main(String[] args) {
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        for (String tag : new String[] {"1", "2"}) {
            System.out.println("sql-before-" + tag + "=" + attempt(() -> describe(platform, "java.sql")));
            System.out.println("listed-before-" + tag + "=" + attempt(() -> listed(platform, "java.sql")));
            try {
                sqlDate = Class.forName("java.sql.Date", false, platform);
            } catch (Throwable t) {
                System.out.println("load-" + tag + "=" + t);
            }
            System.out.println("sql-after-" + tag + "=" + attempt(() -> describe(platform, "java.sql")));
            System.out.println("listed-after-" + tag + "=" + attempt(() -> listed(platform, "java.sql")));
            System.out.println("javax-sql-" + tag + "=" + attempt(() -> describe(platform, "javax.sql")));
            System.out.println("nope-" + tag + "=" + attempt(() -> describe(platform, "java.sql.nope")));
        }
    }
}
