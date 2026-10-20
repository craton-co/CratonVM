// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 93, lane L5 -- under `--jdk-only` the platform
// loader's `getDefinedPackage` sees the package of a class the VM defined for
// it, with no other package question asked in between
// (`i55-L5-proposal-built-in-loaders-answer-packages-from-their-map`, the
// "Wave 92 verification" gap). HotSpot's `BuiltinClassLoader.defineClass`
// ends in `ClassLoader.postDefineClass` -> `getNamedPackage`, which puts the
// package into the loader's `packages` map at the define; the JDK's
// `getDefinedPackage` is `packages.get(name)` then
// `definePackage(name, p.module())`.
//
// Rows:
//   rowset-before    `getDefinedPackage("javax.sql.rowset")` before any class
//                    of it loads
//   rowset-after     the same after `Class.forName("javax.sql.rowset.RowSetProvider",
//                    false, platform)`: the name, identity with the class's
//                    `getPackage()`, and the package's module
//   rowset-again     a second ask answers the same object
//   time-first       `java.sql.Time` loaded, `Time.class.getPackage()` asked
//                    FIRST, then `getDefinedPackage("java.sql")`: the same object
//   defined-by-app   the application loader's `getDefinedPackage("javax.sql.rowset")`:
//                    `null` (it does not delegate)
//   null-name        `getDefinedPackage(null)`: the JDK's NPE text
//
// Run: javac -d out L5W93JdkOnlyDefinedPackage.java
//      java -cp out L5W93JdkOnlyDefinedPackage
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W93JdkOnlyDefinedPackage
//      (`CRATONVM_BUILTIN_LOADER_DEFINED_PACKAGE_SYNC=0` is the kill switch: under
//      `--jdk-only` `rowset-after` then reads `null`.)
//
// HOTSPOT_EXPECTED (derived from the JDK 25 sources, not measured: a plain
// `java -cp out` run loads no `javax.sql.rowset` / `java.sql` class before
// main; `RowSetProvider` is in module `java.sql.rowset`, defined to the
// platform loader):
//   rowset-before=null
//   rowset-after=javax.sql.rowset same=true module=module java.sql.rowset
//   rowset-again=true
//   time-first=java.sql same=true
//   defined-by-app=null
//   null-name=java.lang.NullPointerException msg=name cannot be null

import java.util.concurrent.Callable;

public class L5W93JdkOnlyDefinedPackage {
    static String attempt(Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    public static void main(String[] args) {
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        ClassLoader app = ClassLoader.getSystemClassLoader();
        System.out.println("rowset-before=" + attempt(() -> platform.getDefinedPackage("javax.sql.rowset")));
        System.out.println("rowset-after=" + attempt(() -> {
            Class<?> c = Class.forName("javax.sql.rowset.RowSetProvider", false, platform);
            Package p = platform.getDefinedPackage("javax.sql.rowset");
            if (p == null) {
                return "null";
            }
            return p.getName() + " same=" + (p == c.getPackage()) + " module=" + c.getModule();
        }));
        System.out.println("rowset-again=" + attempt(() ->
                platform.getDefinedPackage("javax.sql.rowset") == platform.getDefinedPackage("javax.sql.rowset")));
        System.out.println("time-first=" + attempt(() -> {
            Class<?> c = Class.forName("java.sql.Time", false, platform);
            Package first = c.getPackage();
            Package p = platform.getDefinedPackage("java.sql");
            if (p == null) {
                return "null";
            }
            return p.getName() + " same=" + (p == first);
        }));
        System.out.println("defined-by-app=" + attempt(() -> app.getDefinedPackage("javax.sql.rowset")));
        System.out.println("null-name=" + attempt(() -> platform.getDefinedPackage(null)));
    }
}
