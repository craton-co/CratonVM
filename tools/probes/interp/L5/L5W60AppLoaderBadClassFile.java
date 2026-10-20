// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L5 (review) -- the application loader
// finds `Bad.class` on the class path and it is not a class file: HotSpot's
// `defineClass` raises `ClassFormatError` and `Class.forName` / `loadClass`
// let it through, as does a child loader's parent delegation. Under
// `--compatible` the served `ClassLoader.loadClass` reported the resolver's
// define failure as "not found" (`classloader_real::load_class_visible_to`'s
// catch-all arm). The first row is repeated last (cold first call).
//
// SETUP and run (the directory holding Bad.class is a second class-path
// entry):
//   mkdir -p badcp && printf 'HTTP/1.1 404 Not Found\n' > badcp/Bad.class
//   javac -d out L5W60AppLoaderBadClassFile.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out:badcp L5W60AppLoaderBadClassFile
// (`;` instead of `:` on Windows.)
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   forname=java.lang.ClassFormatError: Incompatible magic value 1213486160 in class file Bad
//   loadclass=java.lang.ClassFormatError: Incompatible magic value 1213486160 in class file Bad
//   child=java.lang.ClassFormatError: Incompatible magic value 1213486160 in class file Bad
//   absent=java.lang.ClassNotFoundException: Nope
//   forname-again=java.lang.ClassFormatError: Incompatible magic value 1213486160 in class file Bad
//
// CratonVM before the fix (debug build, Windows, base 8004f6b7e1): `--jdk-only`
// as HotSpot; `--compatible` `java.lang.ClassNotFoundException: Bad` on every
// row but `absent`.

import java.net.URL;
import java.net.URLClassLoader;
import java.util.concurrent.Callable;

public class L5W60AppLoaderBadClassFile {
    static String attempt(Callable<Object> c) {
        try {
            Object r = c.call();
            return r instanceof Class<?> k ? k.getName() : String.valueOf(r);
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        System.out.println("forname=" + attempt(() -> Class.forName("Bad", false, app)));
        System.out.println("loadclass=" + attempt(() -> app.loadClass("Bad")));
        System.out.println("child=" + attempt(() -> new URLClassLoader(new URL[0], app).loadClass("Bad")));
        System.out.println("absent=" + attempt(() -> app.loadClass("Nope")));
        System.out.println("forname-again=" + attempt(() -> Class.forName("Bad", false, app)));
    }
}
