// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L5 (review) -- a loader whose parent is
// a BARE `java.net.URLClassLoader` delegates to it first: the parent's own
// URLs answer, and the class is the parent's. `--compatible`'s served
// `ClassLoader.loadClass` asked only a user-defined (subclass) parent, so a
// bare `URLClassLoader` parent was skipped and the flat store answered the
// application loader's copy (or nothing). Self-contained: the probe copies
// its own nested class's bytes to `<tmp>/`, so the class is on the
// application class path AND on the parent's URL; which loader defines it
// tells the paths apart. The first row is repeated last.
//
// Run: javac -d out L5W60UrlParentDelegation.java
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W60UrlParentDelegation
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   null-grandparent=parent
//   platform-grandparent=parent
//   app-grandparent=app
//   subclass-child=parent
//   absent=java.lang.ClassNotFoundException: Nope
//   null-grandparent-again=parent
//
// CratonVM before the fix (debug build, Windows, base 8004f6b7e1):
// `--compatible` `app` on the first, second, fourth and last rows (fixed by
// wave 60, `CRATONVM_CL_DELEGATES_TO_URL_PARENT`). `--jdk-only` as HotSpot
// except `app-grandparent=java.lang.LinkageError: loader Application
// attempted duplicate class definition for L5W60UrlParentDelegation$Inner.`,
// fixed in wave 61 (lane L5, probe `L5W61BootSearchSideDefine`):
// `docs/internal/fixed-bugs/interpreter-L5-jdk-only-a-url-loaders-class-blocks-the-application-loaders-own-define-FIXED-20261025.md`.

import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;

public class L5W60UrlParentDelegation {
    public static class Inner {}

    static final String NAME = "L5W60UrlParentDelegation$Inner";

    static String who(ClassLoader parent, ClassLoader child) {
        try {
            Class<?> c = child.loadClass(NAME);
            ClassLoader l = c.getClassLoader();
            if (l == parent) return "parent";
            if (l == ClassLoader.getSystemClassLoader()) return "app";
            if (l == child) return "child";
            return String.valueOf(l);
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w60urlparent");
        try (InputStream in = L5W60UrlParentDelegation.class.getResourceAsStream(NAME + ".class")) {
            Files.write(dir.resolve(NAME + ".class"), in.readAllBytes());
        }
        URL[] urls = { dir.toUri().toURL() };
        URL[] none = new URL[0];
        ClassLoader app = ClassLoader.getSystemClassLoader();
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        URLClassLoader p1 = new URLClassLoader(urls, null);
        System.out.println("null-grandparent=" + who(p1, new URLClassLoader(none, p1)));
        URLClassLoader p2 = new URLClassLoader(urls, platform);
        System.out.println("platform-grandparent=" + who(p2, new URLClassLoader(none, p2)));
        URLClassLoader p3 = new URLClassLoader(urls, app);
        System.out.println("app-grandparent=" + who(p3, new URLClassLoader(none, p3)));
        URLClassLoader p4 = new URLClassLoader(urls, null);
        System.out.println("subclass-child=" + who(p4, new URLClassLoader(none, p4) {}));
        URLClassLoader p5 = new URLClassLoader(urls, null);
        String absent;
        try {
            new URLClassLoader(none, p5).loadClass("Nope");
            absent = "found";
        } catch (Throwable t) {
            absent = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println("absent=" + absent);
        URLClassLoader p6 = new URLClassLoader(urls, null);
        System.out.println("null-grandparent-again=" + who(p6, new URLClassLoader(none, p6)));
    }
}
