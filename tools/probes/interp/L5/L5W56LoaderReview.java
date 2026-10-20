// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L5 -- a review sweep of the built-in
// loaders' name, resource and package doors, each row what HotSpot's JDK
// body answers. Rows (the first call, and again last):
//   slash-load-*     `app.loadClass("java/lang/String")` (slash form)
//   slash-forname-*  `Class.forName("java/lang/String")`
//   array-load-*     `app.loadClass("[Ljava.lang.String;")` (loadClass has no arrays)
//   array-forname-*  `Class.forName("[Ljava.lang.String;")`
//   space-load-*     `app.loadClass("java.lang.String ")`
//   names-*          the app / platform loaders' `getName()`, the app's parent
//   defined-*        `app.getDefinedPackage("java.lang")`, the platform's
//                    `getDefinedPackage("java.sql")` after loading it
//   jrt-url-*        `getSystemResource("java/lang/Object.class")`'s protocol
//   jrt-stream-*     `Object.class.getResourceAsStream("Object.class")`: the
//                    first four bytes (a named module: no class needed)
//   jrt-app-stream-* `app.getResourceAsStream("java/lang/Object.class")`
//
// Run (no setup):
//   javac -d out L5W56LoaderReview.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W56LoaderReview
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   slash-load-1=java.lang.ClassNotFoundException msg=java/lang/String
//   slash-forname-1=java.lang.ClassNotFoundException msg=java/lang/String
//   array-load-1=java.lang.ClassNotFoundException msg=[Ljava.lang.String;
//   array-forname-1=class [Ljava.lang.String;
//   space-load-1=[java.lang.ClassNotFoundException msg=java.lang.String ]
//   names-1=app platform true null
//   defined-1=null java.sql
//   jrt-url-1=jrt /java.base/java/lang/Object.class
//   jrt-stream-1=cafebabe
//   jrt-app-stream-1=cafebabe
//   slash-load-2=java.lang.ClassNotFoundException msg=java/lang/String
//   slash-forname-2=java.lang.ClassNotFoundException msg=java/lang/String
//   array-load-2=java.lang.ClassNotFoundException msg=[Ljava.lang.String;
//   array-forname-2=class [Ljava.lang.String;
//   space-load-2=[java.lang.ClassNotFoundException msg=java.lang.String ]
//   names-2=app platform true null
//   defined-2=null java.sql
//   jrt-url-2=jrt /java.base/java/lang/Object.class
//   jrt-stream-2=cafebabe
//   jrt-app-stream-2=cafebabe

import java.io.InputStream;
import java.util.concurrent.Callable;

public class L5W56LoaderReview {
    static String attempt(Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    static String head(InputStream in) throws Exception {
        if (in == null) {
            return "null";
        }
        try (in) {
            byte[] b = in.readNBytes(4);
            return String.format("%02x%02x%02x%02x", b[0], b[1], b[2], b[3]);
        }
    }

    static void rows(String tag) {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        System.out.println("slash-load-" + tag + "=" + attempt(() -> app.loadClass("java/lang/String")));
        System.out.println("slash-forname-" + tag + "=" + attempt(() -> Class.forName("java/lang/String")));
        System.out.println("array-load-" + tag + "=" + attempt(() -> app.loadClass("[Ljava.lang.String;")));
        System.out.println("array-forname-" + tag + "=" + attempt(() -> Class.forName("[Ljava.lang.String;")));
        System.out.println("space-load-" + tag + "=[" + attempt(() -> app.loadClass("java.lang.String ")) + "]");
        System.out.println("names-" + tag + "=" + attempt(() ->
                app.getName() + " " + platform.getName() + " " + (app.getParent() == platform)
                        + " " + platform.getParent()));
        System.out.println("defined-" + tag + "=" + attempt(() -> {
            Class.forName("java.sql.Connection");
            Package p = platform.getDefinedPackage("java.sql");
            return app.getDefinedPackage("java.lang") + " " + (p == null ? "null" : p.getName());
        }));
        System.out.println("jrt-url-" + tag + "=" + attempt(() -> {
            java.net.URL u = ClassLoader.getSystemResource("java/lang/Object.class");
            return u == null ? "null" : u.getProtocol() + " " + u.getPath();
        }));
        System.out.println("jrt-stream-" + tag + "=" + attempt(() ->
                head(Object.class.getResourceAsStream("Object.class"))));
        System.out.println("jrt-app-stream-" + tag + "=" + attempt(() ->
                head(app.getResourceAsStream("java/lang/Object.class"))));
    }

    public static void main(String[] args) {
        rows("1");
        rows("2");
    }
}
