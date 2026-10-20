// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, waves 107-108, lane L5 -- see setup.sh for the build,
// the run commands and the expected output. main.jar's `Class-Path: bad.jar
// good.jar`; bad.jar's own `Class-Path: dep.jar C:/nowhere/x.jar` has a token
// with an unknown URL scheme (`c`), whose `new URL` runs
// `URL.lookupViaProviders`: a ServiceLoader over the system loader, whose
// resource walk re-enters the same application `URLClassPath` and opens
// good.jar, whose `http:` token needs a provider lookup too -> HotSpot throws
// `Error("Circular loading of URL stream handler providers detected")` into
// the `bad.B` lookup, and the application loader loses both jars. Over a
// URLClassLoader of their own (rows 6-9) there is no circularity: bad.jar is
// skipped whole, good.jar's `http:` token is only ignored.

import java.io.File;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;

public class L5W107ClassPathDriveToken {
    static String cls(ClassLoader loader, String name) {
        try {
            Class<?> c = Class.forName(name, false, loader);
            ClassLoader l = c.getClassLoader();
            String who = l == ClassLoader.getSystemClassLoader() ? "app"
                    : l == loader ? "url" : String.valueOf(l);
            return name + "=found loader=" + who;
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
        ClassLoader app = L5W107ClassPathDriveToken.class.getClassLoader();
        System.out.println("app " + cls(app, "bad.B"));
        System.out.println("app " + cls(app, "dep.D"));
        System.out.println("app " + cls(app, "good.G"));
        System.out.println("app " + res(app, "bad/b.txt"));
        System.out.println("app " + res(app, "good/g.txt"));

        String cp = System.getProperty("java.class.path");
        File dir = new File(cp.split(File.pathSeparator)[0]).getAbsoluteFile().getParentFile();
        for (String jar : new String[] {"bad.jar", "good.jar"}) {
            URL url = new File(dir, jar).toURI().toURL();
            try (URLClassLoader l = new URLClassLoader(new URL[] {url}, null)) {
                String cn = jar.equals("bad.jar") ? "bad.B" : "good.G";
                String rn = jar.equals("bad.jar") ? "bad/b.txt" : "good/g.txt";
                System.out.println(jar + " " + cls(l, cn));
                System.out.println(jar + " " + res(l, rn));
            }
        }
    }
}
