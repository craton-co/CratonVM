// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 96, lane L3
// (`interpreter-i95-L3-compatible-builtin-loader-find-resource-by-module-is-null-FIXED-20261130.md`):
// `Class.getResource` of a class of a named PLATFORM or APPLICATION system
// module. JDK 25's body calls `BuiltinClassLoader.findResource(mn, name)`,
// which asks the loader's `nameToModule`. Only `--jdk-only` fills that map, so
// `--compatible` answered null. HotSpot answers module `mn`'s
// `ModuleReader.find(name)`: the `jrt:` URL of a resource the module holds,
// null for one it does not (even when another module holds it).
//
// Run (both modes must print the HotSpot lines; no arguments, no setup):
//   javac -d out L3W96PlatformModuleResource.java
//   cratonvm --jdk-only   -cp out L3W96PlatformModuleResource
//   cratonvm --compatible -cp out L3W96PlatformModuleResource
//   java                  -cp out L3W96PlatformModuleResource
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; derived from the JDK 25
// sources: java.sql / java.net.http are in ModuleLoaderMap's platform set,
// jdk.compiler in neither set (application loader) and a root of the default
// unnamed-module boot layer because it exports packages unqualified)
// sql-date-class: found
// sql-date-url: jrt:/java.sql/java/sql/Date.class
// sql-date-loader: platform
// sql-date-stream: found
// sql-other-module: null
// sql-missing: null
// http-client-class: found
// javac-main-loader: app
// javac-main-class: found
// javac-main-url: jrt:/jdk.compiler/com/sun/tools/javac/Main.class
// HOTSPOT_EXPECTED_END
import java.io.InputStream;
import java.net.URL;

public class L3W96PlatformModuleResource {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getMessage() == null ? t.getClass().getName()
                    : t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static String found(URL u) {
        return u == null ? "null" : "found";
    }

    static String loaderKind(Class<?> c) {
        ClassLoader cl = c.getClassLoader();
        if (cl == null) return "boot";
        if (cl == ClassLoader.getPlatformClassLoader()) return "platform";
        if (cl == ClassLoader.getSystemClassLoader()) return "app";
        return "other";
    }

    public static void main(String[] args) throws Exception {
        Class<?> date = java.sql.Date.class;
        row("sql-date-class", () -> found(date.getResource("Date.class")));
        row("sql-date-url", () -> date.getResource("Date.class"));
        row("sql-date-loader", () -> loaderKind(date));
        row("sql-date-stream", () -> {
            try (InputStream in = date.getResourceAsStream("Date.class")) {
                return in == null ? "null" : "found";
            }
        });
        // java.base's class asked through java.sql: not java.sql's resource.
        row("sql-other-module", () -> found(date.getResource("/java/lang/Object.class")));
        row("sql-missing", () -> found(date.getResource("NoSuchClass96.class")));
        row("http-client-class",
                () -> found(java.net.http.HttpClient.class.getResource("HttpClient.class")));
        Class<?> javac = Class.forName("com.sun.tools.javac.Main");
        row("javac-main-loader", () -> loaderKind(javac));
        row("javac-main-class", () -> found(javac.getResource("Main.class")));
        row("javac-main-url", () -> javac.getResource("Main.class"));
    }
}
