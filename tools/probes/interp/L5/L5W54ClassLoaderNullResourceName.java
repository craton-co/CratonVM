// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 54, lane L5 -- a `ClassLoader` resource lookup of
// a `null` name raises `Objects.requireNonNull`'s NullPointerException: no
// message, `requireNonNull` on top of the served `ClassLoader` method.
// CratonVM's resource natives (`native-builtins/src/classloader.rs`
// `cl_get_resource`, `cl_get_resources`, `cl_get_resource_as_stream`) built
// the NPE themselves with a message ("ClassLoader.getResource name is null"),
// so the capture listed the probe on top
// (`docs/internal/fixed-bugs/interpreter-L5-classloader-resource-lookups-of-a-null-name-raise-the-vms-own-npe-FIXED-20261018.md`).
// Measured on the base (a 2026-09-25 release build, Windows): every `app-*`
// row and `sys-*` row (both modes) and, under `--compatible`, every `user-*`
// row printed the VM's message and the probe's lambda on top, and every
// `override-*` row a `ClassLoader` NPE (both modes).
//
// Rows (the null-name rows first, on a cold VM, and again last):
//   app-*   the application class loader
//   user-*  a `URLClassLoader` over nothing, parented by the application loader
//   sys-*   the static `ClassLoader.getSystemResource(null)` (its HotSpot trace
//           has a second JDK hop; the message must still be null)
//   override-* a user loader whose `getResource` / `getResourceAsStream`
//           overrides answer a null name themselves: HotSpot runs them.
//           CratonVM's `invoke_or_native` null-name arm ran `ClassLoader`'s
//           native in their place (and in place of
//           `URLClassLoader.getResourceAsStream`): `vm_exec.rs`
//           `receiver_inherits_class_loader_method`.
//   control a present resource (this probe's own class file)
// Each line prints the throwable's class, its message and the top two frames
// as `Class.method` (no line numbers).
//
// Run (no setup):
//   javac -d out L5W54ClassLoaderNullResourceName.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W54ClassLoaderNullResourceName
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   app-getResource-1=java.lang.NullPointerException msg=null top=java.util.Objects.requireNonNull<java.lang.ClassLoader.getResource
//   app-getResources-1=java.lang.NullPointerException msg=null top=java.util.Objects.requireNonNull<java.lang.ClassLoader.getResources
//   app-getResourceAsStream-1=java.lang.NullPointerException msg=null top=java.util.Objects.requireNonNull<java.lang.ClassLoader.getResourceAsStream
//   user-getResource-1=java.lang.NullPointerException msg=null top=java.util.Objects.requireNonNull<java.lang.ClassLoader.getResource
//   user-getResources-1=java.lang.NullPointerException msg=null top=java.util.Objects.requireNonNull<java.lang.ClassLoader.getResources
//   user-getResourceAsStream-1=java.lang.NullPointerException msg=null top=java.util.Objects.requireNonNull<java.net.URLClassLoader.getResourceAsStream
//   sys-getSystemResource-1=java.lang.NullPointerException msg=null
//   override-getResource-1=no-throw true
//   override-getResourceAsStream-1=no-throw true
//   control=found=true
//   app-getResource-2=java.lang.NullPointerException msg=null top=java.util.Objects.requireNonNull<java.lang.ClassLoader.getResource
//   app-getResources-2=java.lang.NullPointerException msg=null top=java.util.Objects.requireNonNull<java.lang.ClassLoader.getResources
//   app-getResourceAsStream-2=java.lang.NullPointerException msg=null top=java.util.Objects.requireNonNull<java.lang.ClassLoader.getResourceAsStream
//   user-getResource-2=java.lang.NullPointerException msg=null top=java.util.Objects.requireNonNull<java.lang.ClassLoader.getResource
//   user-getResources-2=java.lang.NullPointerException msg=null top=java.util.Objects.requireNonNull<java.lang.ClassLoader.getResources
//   user-getResourceAsStream-2=java.lang.NullPointerException msg=null top=java.util.Objects.requireNonNull<java.net.URLClassLoader.getResourceAsStream
//   sys-getSystemResource-2=java.lang.NullPointerException msg=null
//   override-getResource-2=no-throw true
//   override-getResourceAsStream-2=no-throw true

import java.net.URL;
import java.net.URLClassLoader;

public class L5W54ClassLoaderNullResourceName {
    interface Lookup {
        Object run() throws Exception;
    }

    static String top(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 2 && i < st.length; i++) {
            if (i > 0) {
                sb.append('<');
            }
            sb.append(st[i].getClassName()).append('.').append(st[i].getMethodName());
        }
        return sb.toString();
    }

    static void row(String label, boolean frames, Lookup lookup) {
        try {
            Object r = lookup.run();
            System.out.println(label + "=no-throw " + (r != null));
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName() + " msg=" + t.getMessage()
                    + (frames ? " top=" + top(t) : ""));
        }
    }

    static void round(int n) throws Exception {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        row("app-getResource-" + n, true, () -> app.getResource(null));
        row("app-getResources-" + n, true, () -> app.getResources(null));
        row("app-getResourceAsStream-" + n, true, () -> app.getResourceAsStream(null));
        try (URLClassLoader user = new URLClassLoader(new URL[0], app)) {
            row("user-getResource-" + n, true, () -> user.getResource(null));
            row("user-getResources-" + n, true, () -> user.getResources(null));
            row("user-getResourceAsStream-" + n, true, () -> user.getResourceAsStream(null));
        }
        row("sys-getSystemResource-" + n, false, () -> ClassLoader.getSystemResource(null));
        Lenient lenient = new Lenient(app);
        row("override-getResource-" + n, true, () -> lenient.getResource(null));
        row("override-getResourceAsStream-" + n, true, () -> lenient.getResourceAsStream(null));
    }

    /** A loader whose overrides answer a null name themselves. */
    static final class Lenient extends ClassLoader {
        Lenient(ClassLoader parent) {
            super(parent);
        }

        @Override
        public URL getResource(String name) {
            return name == null ? getParent().getResource("L5W54ClassLoaderNullResourceName.class")
                    : super.getResource(name);
        }

        @Override
        public java.io.InputStream getResourceAsStream(String name) {
            return name == null ? new java.io.ByteArrayInputStream(new byte[1]) : super.getResourceAsStream(name);
        }
    }

    public static void main(String[] args) throws Exception {
        round(1);
        URL self = ClassLoader.getSystemClassLoader()
                .getResource("L5W54ClassLoaderNullResourceName.class");
        System.out.println("control=found=" + (self != null));
        round(2);
    }
}
