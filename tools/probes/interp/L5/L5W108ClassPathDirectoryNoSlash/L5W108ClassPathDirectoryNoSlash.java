// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 108, lane L5 -- see setup.sh for the build, the
// run commands and the expected output. main.jar's `Class-Path: classes
// extra/ dot/.` names three directories. JDK 25's `URLClassPath.getLoader`
// makes a directory loader only for a URL ending in `/` (`dot/.` folds to
// `dot/`); `classes` becomes a JarLoader that cannot open a directory, and
// the URL is skipped.

import java.io.InputStream;

public class L5W108ClassPathDirectoryNoSlash {
    static String cls(ClassLoader loader, String name) {
        try {
            Class.forName(name, false, loader);
            return name + "=found";
        } catch (Throwable t) {
            return name + "=" + t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static String res(ClassLoader loader, String name) throws Exception {
        try (InputStream in = loader.getResourceAsStream(name)) {
            return name + "=" + (in == null ? "null" : new String(in.readAllBytes(), "UTF-8"));
        }
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = L5W108ClassPathDirectoryNoSlash.class.getClassLoader();
        System.out.println(cls(app, "c.C"));
        System.out.println(cls(app, "e.E"));
        System.out.println(cls(app, "d.D"));
        System.out.println(res(app, "c/c.txt"));
        System.out.println(res(app, "e/e.txt"));
        System.out.println(res(app, "d/d.txt"));
    }
}
