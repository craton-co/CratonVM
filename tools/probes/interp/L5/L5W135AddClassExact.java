// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i7 wave 135, lane L5
// (`docs/feature-designs/real-jdk-mode.md`):
// HotSpot's define records every class a loader object defines with
// `java.lang.ClassLoader.addClass(Class)` called EXACTLY (package-private in
// java.lang: no loader class can override it). A user loader that declares
// its own `addClass(Class)` -- package-private, public or private -- never
// sees that call. CratonVM's define doors made the upcall through the plain
// virtual door, which runs the first same-signature method from the
// receiver's class down, so the loader's own method ran once per define.
// `CRATONVM_DEFINE_ADD_CLASS_EXACT=0` restores that (the negative control:
// `calls=1` on the pkg-private, public and url rows; the private row depends
// on whether the receiver walk takes a private method);
// `CRATONVM_DEFINE_ADD_CLASS=0` makes no upcall at all (the rows read
// `calls=0` too, so it is not a control).
//
// Run: javac -d out L5W135AddClassExact.java
//      java -cp out L5W135AddClassExact                     (HotSpot reference)
//      cratonvm --real-jdk -cp out L5W135AddClassExact
//      cratonvm --compatible -cp out L5W135AddClassExact
//      CRATONVM_DEFINE_ADD_CLASS_EXACT=0 cratonvm --real-jdk -cp out L5W135AddClassExact   (control)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// pkg-private defineClass defined=true calls=0
// public defineClass defined=true calls=0
// private defineClass defined=true calls=0
// url-public loadClass defined=true own-loader=true calls=0
// HOTSPOT_EXPECTED_END

import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;

public class L5W135AddClassExact {
    /** The class every loader below defines a copy of. */
    public static class Payload {
        public static int answer() {
            return 42;
        }
    }

    static final String PAYLOAD = "L5W135AddClassExact$Payload";

    static byte[] payloadBytes() throws Exception {
        try (InputStream in = L5W135AddClassExact.class.getResourceAsStream("/" + PAYLOAD + ".class")) {
            return in.readAllBytes();
        }
    }

    /** Declares a package-private `addClass(Class)` of its own. */
    static class PkgLoader extends ClassLoader {
        int calls;

        PkgLoader() {
            super(L5W135AddClassExact.class.getClassLoader());
        }

        void addClass(Class<?> c) {
            calls++;
        }

        Class<?> def(byte[] b) {
            return defineClass(PAYLOAD, b, 0, b.length);
        }
    }

    /** Declares a public `addClass(Class)` of its own. */
    static class PublicLoader extends ClassLoader {
        int calls;

        PublicLoader() {
            super(L5W135AddClassExact.class.getClassLoader());
        }

        public void addClass(Class<?> c) {
            calls++;
        }

        Class<?> def(byte[] b) {
            return defineClass(PAYLOAD, b, 0, b.length);
        }
    }

    /** Declares a private `addClass(Class)` of its own. */
    static class PrivateLoader extends ClassLoader {
        int calls;

        PrivateLoader() {
            super(L5W135AddClassExact.class.getClassLoader());
        }

        private void addClass(Class<?> c) {
            calls++;
        }

        int seen() {
            return calls;
        }

        Class<?> def(byte[] b) {
            return defineClass(PAYLOAD, b, 0, b.length);
        }
    }

    /** A `URLClassLoader` over this probe's own directory, boot parent. */
    static class UrlLoader extends URLClassLoader {
        int calls;

        UrlLoader(URL[] urls) {
            super(urls, null);
        }

        public void addClass(Class<?> c) {
            calls++;
        }
    }

    public static void main(String[] args) throws Exception {
        byte[] bytes = payloadBytes();

        PkgLoader pkg = new PkgLoader();
        Class<?> a = pkg.def(bytes);
        System.out.println("pkg-private defineClass defined=" + (a.getClassLoader() == pkg)
                + " calls=" + pkg.calls);

        PublicLoader pub = new PublicLoader();
        Class<?> b = pub.def(bytes);
        System.out.println("public defineClass defined=" + (b.getClassLoader() == pub)
                + " calls=" + pub.calls);

        PrivateLoader priv = new PrivateLoader();
        Class<?> c = priv.def(bytes);
        System.out.println("private defineClass defined=" + (c.getClassLoader() == priv)
                + " calls=" + priv.seen());

        URL dir = L5W135AddClassExact.class.getProtectionDomain().getCodeSource().getLocation();
        try (UrlLoader url = new UrlLoader(new URL[] {dir})) {
            Class<?> d = url.loadClass(PAYLOAD);
            Object answer = d.getMethod("answer").invoke(null);
            System.out.println("url-public loadClass defined=" + Integer.valueOf(42).equals(answer)
                    + " own-loader=" + (d.getClassLoader() == url) + " calls=" + url.calls);
        }
    }
}
