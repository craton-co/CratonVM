// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i7 wave 136, lane L5 (page
// `i136-L5-compatible-url-class-loader-find-class-answers-the-app-loaders-class`):
// `URLClassLoader.findClass` searches the loader's own URLs and nothing else.
//
// A `URLClassLoader` subclass whose parent is the application loader calls
// `findClass` directly (a child-first loader does this before delegating).
// HotSpot: a class on the application class path but not in the loader's URLs
// is `ClassNotFoundException`; a class in its URLs is defined by the loader
// itself. CratonVM's `--compatible` stand-in (`ucl_real_find_class`) fell back
// to the global class store after its local search missed and answered the
// application loader's class (row `find-app-class`).
//
// Rows:
//   * `find-app-class:` findClass of a class only the application loader has;
//   * `find-jdk-class:` findClass of `java.lang.String`;
//   * `find-missing:` findClass of a name nobody has;
//   * `find-local:` findClass of a class copied into the loader's URL dir
//     (defined by the loader, a different class from the app's copy);
//   * `child-first-app:` a child-first `loadClass` (findClass, then parent) of
//     the app-only class: the application loader's class.
//
// Run: javac -d out L5W136UrlFindClassChildFirst.java
//      cratonvm [--real-jdk|--compatible] [--nojit] -cp out L5W136UrlFindClassChildFirst
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3)
// find-app-class: java.lang.ClassNotFoundException: L5W136UrlFindClassChildFirst$Peer
// find-jdk-class: java.lang.ClassNotFoundException: java.lang.String
// find-missing: java.lang.ClassNotFoundException: no.such.L5W136Klass
// find-local: loader=self sameAsApp=false
// child-first-app: loader=app
// HOTSPOT_EXPECTED_END
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;

public class L5W136UrlFindClassChildFirst {
    public static class Peer {}

    public static class Local {}

    static final class ChildFirst extends URLClassLoader {
        ChildFirst(URL[] urls, ClassLoader parent) {
            super(urls, parent);
        }

        Class<?> find(String name) throws ClassNotFoundException {
            return findClass(name);
        }

        Class<?> childFirst(String name) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                try {
                    return findClass(name);
                } catch (ClassNotFoundException e) {
                    return getParent().loadClass(name);
                }
            }
        }
    }

    static String who(ChildFirst cl, Class<?> c) {
        ClassLoader l = c.getClassLoader();
        if (l == cl) {
            return "self";
        }
        if (l == L5W136UrlFindClassChildFirst.class.getClassLoader()) {
            return "app";
        }
        return String.valueOf(l);
    }

    static String find(ChildFirst cl, String name) {
        try {
            Class<?> c = cl.find(name);
            return "loader=" + who(cl, c);
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w136");
        String localFile = "L5W136UrlFindClassChildFirst$Local.class";
        try (InputStream in =
                L5W136UrlFindClassChildFirst.class.getClassLoader().getResourceAsStream(localFile)) {
            Files.write(dir.resolve(localFile), in.readAllBytes());
        }
        ChildFirst cl = new ChildFirst(
                new URL[] {dir.toUri().toURL()}, L5W136UrlFindClassChildFirst.class.getClassLoader());
        System.out.println("find-app-class: " + find(cl, Peer.class.getName()));
        System.out.println("find-jdk-class: " + find(cl, "java.lang.String"));
        System.out.println("find-missing: " + find(cl, "no.such.L5W136Klass"));
        try {
            Class<?> local = cl.find(Local.class.getName());
            System.out.println("find-local: loader=" + who(cl, local) + " sameAsApp=" + (local == Local.class));
        } catch (Throwable t) {
            System.out.println("find-local: " + t.getClass().getName() + ": " + t.getMessage());
        }
        try {
            Class<?> c = cl.childFirst(Peer.class.getName());
            System.out.println("child-first-app: loader=" + who(cl, c));
        } catch (Throwable t) {
            System.out.println("child-first-app: " + t.getClass().getName() + ": " + t.getMessage());
        }
        Files.deleteIfExists(dir.resolve(localFile));
        Files.deleteIfExists(dir);
    }
}
