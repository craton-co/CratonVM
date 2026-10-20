// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L5 (review) -- a class file a loader
// FINDS but cannot define is `defineClass`'s error, never "not found": a
// file that is not a class file (`ClassFormatError`), and a class file of a
// newer release (`UnsupportedClassVersionError`), through a
// `URLClassLoader`'s own search (null parent, and the application loader as
// parent) and `Class.forName` on it. Self-contained: the probe writes the
// files to a temporary directory (the second from its own nested class's
// bytes, major version patched to 99). The first row is repeated last.
//
// Run: javac -d out L5W60LoaderBadClassFile.java
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W60LoaderBadClassFile
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   garbage=java.lang.ClassFormatError: Incompatible magic value 1213486160 in class file Bad
//   garbage-app-parent=java.lang.ClassFormatError: Incompatible magic value 1213486160 in class file Bad
//   garbage-forname=java.lang.ClassFormatError: Incompatible magic value 1213486160 in class file Bad
//   newer=java.lang.UnsupportedClassVersionError: L5W60LoaderBadClassFile$Inner has been compiled by a more recent version of the Java Runtime (class file version 99.0), this version of the Java Runtime only recognizes class file versions up to 69.0
//   newer-subclass=java.lang.UnsupportedClassVersionError: L5W60LoaderBadClassFile$Inner has been compiled by a more recent version of the Java Runtime (class file version 99.0), this version of the Java Runtime only recognizes class file versions up to 69.0
//   garbage-again=java.lang.ClassFormatError: Incompatible magic value 1213486160 in class file Bad

import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.concurrent.Callable;

public class L5W60LoaderBadClassFile {
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
        Path garbageDir = Files.createTempDirectory("l5w60bad");
        Files.writeString(garbageDir.resolve("Bad.class"), "HTTP/1.1 404 Not Found\n");
        Path newerDir = Files.createTempDirectory("l5w60newer");
        byte[] bytes;
        try (InputStream in = L5W60LoaderBadClassFile.class
                .getResourceAsStream("L5W60LoaderBadClassFile$Inner.class")) {
            bytes = in.readAllBytes();
        }
        bytes[6] = 0;
        bytes[7] = 99;
        Files.write(newerDir.resolve("L5W60LoaderBadClassFile$Inner.class"), bytes);
        URL[] garbage = { garbageDir.toUri().toURL() };
        URL[] newer = { newerDir.toUri().toURL() };
        ClassLoader app = ClassLoader.getSystemClassLoader();
        System.out.println("garbage=" + attempt(() -> new URLClassLoader(garbage, null).loadClass("Bad")));
        System.out.println("garbage-app-parent=" + attempt(() -> new URLClassLoader(garbage, app).loadClass("Bad")));
        System.out.println("garbage-forname=" + attempt(() -> Class.forName("Bad", false, new URLClassLoader(garbage, null))));
        System.out.println("newer=" + attempt(() -> new URLClassLoader(newer, null).loadClass("L5W60LoaderBadClassFile$Inner")));
        // An anonymous subclass (null parent) takes the base `loadClass` / `findClass`.
        System.out.println("newer-subclass=" + attempt(() -> new URLClassLoader(newer, null) {
        }.loadClass("L5W60LoaderBadClassFile$Inner")));
        System.out.println("garbage-again=" + attempt(() -> new URLClassLoader(garbage, null).loadClass("Bad")));
    }
}
