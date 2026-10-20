// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L5 -- a class file found under the
// requested name that declares another class is the definer's
// `NoClassDefFoundError: <requested> (wrong name: <declared>)`, through every
// built-in door: a `URLClassLoader`'s own `findClass` (null parent, and the
// application loader as parent, which misses first), and a child loader
// whose parent is such a `URLClassLoader` (delegation lets the error out:
// `ClassLoader.loadClass` catches `ClassNotFoundException` only). The first
// row is repeated last (cold first call). Self-contained: the probe copies
// its own nested class's bytes to `<tmp>/Inner.class`.
//
// Run: javac -d out L5W60WrongNameDelegation.java
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W60WrongNameDelegation
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   null-parent=java.lang.NoClassDefFoundError: Inner (wrong name: L5W60WrongNameDelegation$Inner)
//   app-parent=java.lang.NoClassDefFoundError: Inner (wrong name: L5W60WrongNameDelegation$Inner)
//   child-of-url=java.lang.NoClassDefFoundError: Inner (wrong name: L5W60WrongNameDelegation$Inner)
//   forname-url=java.lang.NoClassDefFoundError: Inner (wrong name: L5W60WrongNameDelegation$Inner)
//   absent=java.lang.ClassNotFoundException: Nope
//   null-parent-again=java.lang.NoClassDefFoundError: Inner (wrong name: L5W60WrongNameDelegation$Inner)
//
// CratonVM before the fix (debug build, Windows, base 8004f6b7e1): see
// `docs/internal/fixed-bugs/interpreter-L5-the-application-loader-answers-a-wrong-name-class-file-with-classnotfound-FIXED-20261024.md`.

import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.Callable;

public class L5W60WrongNameDelegation {
    public static class Inner {}

    static String attempt(Callable<Object> c) {
        try {
            Object r = c.call();
            return r instanceof Class<?> k ? k.getName() : String.valueOf(r);
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w60wrongname");
        try (InputStream in = L5W60WrongNameDelegation.class
                .getResourceAsStream("L5W60WrongNameDelegation$Inner.class")) {
            Files.write(dir.resolve("Inner.class"), in.readAllBytes());
        }
        URL[] urls = { dir.toUri().toURL() };
        ClassLoader app = ClassLoader.getSystemClassLoader();
        System.out.println("null-parent=" + attempt(() -> new URLClassLoader(urls, null).loadClass("Inner")));
        System.out.println("app-parent=" + attempt(() -> new URLClassLoader(urls, app).loadClass("Inner")));
        URLClassLoader parent = new URLClassLoader(urls, null);
        System.out.println("child-of-url=" + attempt(() -> new URLClassLoader(new URL[0], parent).loadClass("Inner")));
        System.out.println("forname-url=" + attempt(() -> Class.forName("Inner", false, new URLClassLoader(urls, null))));
        System.out.println("absent=" + attempt(() -> new URLClassLoader(urls, null).loadClass("Nope")));
        System.out.println("null-parent-again=" + attempt(() -> new URLClassLoader(urls, null).loadClass("Inner")));
    }
}
