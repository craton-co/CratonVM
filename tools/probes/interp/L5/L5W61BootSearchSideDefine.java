// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 61, lane L5 -- the boot search for a name some
// URL loader already defined is a lookup, never a define: it must leave the
// application loader free to define its own class of that name
// (`interpreter-L5-jdk-only-a-url-loaders-class-blocks-the-application-loaders-own-define`).
//
// Per class: a bare `URLClassLoader` (null parent) over a copy of the class
// file defines its own copy; the platform loader is then asked for the name
// (its `loadClassOrNull` searches the boot loader, `findBootstrapClass`); then
// the application loader resolves the name for the first time
// (`Class.forName`, and a `URLClassLoader` child of the application loader).
// Before wave 61, `--jdk-only` answered the boot search by LOADING the name
// from the class path under `Application` (then answering null), so the
// application loader's own `defineClass` failed as a duplicate:
// `java.lang.LinkageError: loader Application attempted duplicate class
// definition for L5W61BootSearchSideDefine$One.` The second class repeats the
// rows (cold first call).
//
// Run: javac -d out L5W61BootSearchSideDefine.java
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W61BootSearchSideDefine
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   One url=url
//   One platform=java.lang.ClassNotFoundException: L5W61BootSearchSideDefine$One
//   One forName=app
//   One app-child=app
//   Two url=url
//   Two platform=java.lang.ClassNotFoundException: L5W61BootSearchSideDefine$Two
//   Two forName=app
//   Two app-child=app

import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.Callable;

public class L5W61BootSearchSideDefine {
    public static class One {}
    public static class Two {}

    static String who(Callable<Class<?>> c, ClassLoader url) {
        try {
            ClassLoader l = c.call().getClassLoader();
            if (l == ClassLoader.getSystemClassLoader()) return "app";
            if (l == url) return "url";
            return String.valueOf(l);
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static void rows(String simple) throws Exception {
        String name = "L5W61BootSearchSideDefine$" + simple;
        Path dir = Files.createTempDirectory("l5w61boot");
        try (InputStream in = L5W61BootSearchSideDefine.class.getResourceAsStream(name + ".class")) {
            Files.write(dir.resolve(name + ".class"), in.readAllBytes());
        }
        URL[] urls = { dir.toUri().toURL() };
        URLClassLoader url = new URLClassLoader(urls, null);
        ClassLoader app = ClassLoader.getSystemClassLoader();
        System.out.println(simple + " url=" + who(() -> url.loadClass(name), url));
        System.out.println(simple + " platform="
                + who(() -> ClassLoader.getPlatformClassLoader().loadClass(name), url));
        System.out.println(simple + " forName=" + who(() -> Class.forName(name, false, app), url));
        System.out.println(simple + " app-child="
                + who(() -> new URLClassLoader(new URL[0], app).loadClass(name), url));
    }

    public static void main(String[] args) throws Exception {
        rows("One");
        rows("Two");
    }
}
