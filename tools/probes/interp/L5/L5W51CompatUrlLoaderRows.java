// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 51, lane L5 -- the `URLClassLoader` gates of
// `docs/internal/fixed-bugs/interpreter-L5-compatible-gates-that-still-differ-after-the-alignment-table-FIXED-20261007.md`
// (items 6 and 8), one row each, the case first (a cold VM) and again last
// on fresh loaders over fresh directories:
//
//   iso-static  an isolated `URLClassLoader` (parent null) over a directory
//               holding `$Iso` only; `Iso.callStatic()` does `invokestatic
//               $Helper.v()`, a class on the application class path only.
//               HotSpot: `NoClassDefFoundError` (the isolated loader's miss
//               is final). Item 8: `--compatible` let the flat store answer
//               the application's `$Helper` after the loader refused, once
//               the `loader-throws` row is off (with it on, the loader's own
//               `ClassNotFoundException` already fails the resolution).
//   iso-new     the same reference through `new $Helper2()` (the
//               `CONSTANT_Class` door; the control, both modes refused it).
//   iso-pd      `$Iso`'s loader and `CodeSource` location (item 6; matches
//               HotSpot in both modes already).
//   handler     a `URLClassLoader` over `l5w51mem:/root/`, a URL whose
//               `URLStreamHandler` is the application's and serves `$Iso`'s
//               bytes from memory, and a resource. HotSpot reads both through
//               the handler. Item 6: the `--compatible` stand-in reads each
//               URL back as a filesystem path and finds nothing --
//               `handler-N=load java.lang.ClassNotFoundException opened=false
//               resource=null text=none`. Wave 52 (lane L5): the
//               `jdk-url-class-loaders` HotSpot-alignment row runs the JDK's
//               `URLClassLoader` bodies under `--compatible` too (over a real
//               JDK, not yet on Windows: the
//               `i52-L5-compatible-file-keeps-a-slash-before-a-windows-drive`
//               page), and the rows match HotSpot there; with
//               `CRATONVM_COMPAT_ALIGN=-jdk-url-class-loaders` they differ as
//               above.
//
// Run (no setup):
//   javac -d out L5W51CompatUrlLoaderRows.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W51CompatUrlLoaderRows
//
// Positive control (`--compatible`): `CRATONVM_COMPAT_ALIGN=-loader-throws,-isolated-static-owner`
// prints `iso-static-N=app`, and `-loader-throws,+isolated-static-owner`
// HotSpot's lines; with `CRATONVM_DBG=access` the exit line `[ACCESS-DBG]
// ALIGNMENT census: ...` counts `isolated-static-owner=2` in the second run.
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   iso-static-1=java.lang.NoClassDefFoundError: L5W51CompatUrlLoaderRows$Helper
//   iso-new-1=java.lang.NoClassDefFoundError: L5W51CompatUrlLoaderRows$Helper2
//   iso-pd-1=loader=true location=true again=true
//   handler-1=loader=true location=l5w51mem:/root/ opened=true resource=l5w51mem:/root/l5w51/res.txt text=hello
//   iso-static-2=java.lang.NoClassDefFoundError: L5W51CompatUrlLoaderRows$Helper
//   iso-new-2=java.lang.NoClassDefFoundError: L5W51CompatUrlLoaderRows$Helper2
//   iso-pd-2=loader=true location=true again=true
//   handler-2=loader=true location=l5w51mem:/root/ opened=true resource=l5w51mem:/root/l5w51/res.txt text=hello

import java.io.ByteArrayInputStream;
import java.io.FileNotFoundException;
import java.io.IOException;
import java.io.InputStream;
import java.lang.reflect.InvocationTargetException;
import java.net.URL;
import java.net.URLClassLoader;
import java.net.URLConnection;
import java.net.URLStreamHandler;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashMap;
import java.util.Map;

public class L5W51CompatUrlLoaderRows {
    static final String P = "L5W51CompatUrlLoaderRows$";

    public static class Helper {
        public static String v() {
            return "app";
        }
    }

    public static class Helper2 {
    }

    public static class Iso {
        public static String callStatic() {
            return Helper.v();
        }

        public static Object callNew() {
            return new Helper2();
        }
    }

    static byte[] appBytes(String name) throws IOException {
        try (InputStream in = ClassLoader.getSystemResourceAsStream(name + ".class")) {
            return in.readAllBytes();
        }
    }

    static String call(Class<?> c, String m) {
        try {
            return String.valueOf(c.getMethod(m).invoke(null));
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            return t.getClass().getName() + ": " + t.getMessage();
        } catch (Throwable t) {
            return "unexpected " + t;
        }
    }

    static void isolated(int round) throws Exception {
        Path dir = Files.createTempDirectory("l5w51");
        Files.write(dir.resolve(P + "Iso.class"), appBytes(P + "Iso"));
        URL url = dir.toUri().toURL();
        try (URLClassLoader u = new URLClassLoader(new URL[] {url}, null)) {
            Class<?> iso = u.loadClass(P + "Iso");
            System.out.println("iso-static-" + round + "=" + call(iso, "callStatic"));
            System.out.println("iso-new-" + round + "=" + call(iso, "callNew"));
            URL loc = iso.getProtectionDomain().getCodeSource() == null
                    ? null
                    : iso.getProtectionDomain().getCodeSource().getLocation();
            System.out.println("iso-pd-" + round + "=loader=" + (iso.getClassLoader() == u)
                    + " location=" + url.equals(loc)
                    + " again=" + (u.loadClass(P + "Iso") == iso));
        }
    }

    static final class MemHandler extends URLStreamHandler {
        final Map<String, byte[]> files = new HashMap<>();
        int opened;

        @Override
        protected URLConnection openConnection(URL u) {
            String path = u.getPath();
            return new URLConnection(u) {
                @Override
                public void connect() {
                }

                @Override
                public InputStream getInputStream() throws IOException {
                    byte[] b = files.get(path);
                    if (b == null) {
                        throw new FileNotFoundException(path);
                    }
                    opened++;
                    return new ByteArrayInputStream(b);
                }
            };
        }
    }

    @SuppressWarnings("deprecation")
    static void handler(int round) throws Exception {
        MemHandler h = new MemHandler();
        h.files.put("/root/" + P + "Iso.class", appBytes(P + "Iso"));
        h.files.put("/root/l5w51/res.txt", "hello".getBytes(StandardCharsets.UTF_8));
        URL base = new URL(null, "l5w51mem:/root/", h);
        try (URLClassLoader u = new URLClassLoader(new URL[] {base}, null)) {
            String line;
            try {
                Class<?> iso = u.loadClass(P + "Iso");
                URL loc = iso.getProtectionDomain().getCodeSource() == null
                        ? null
                        : iso.getProtectionDomain().getCodeSource().getLocation();
                line = "loader=" + (iso.getClassLoader() == u) + " location=" + loc;
            } catch (Throwable t) {
                line = "load " + t.getClass().getName();
            }
            URL res = u.getResource("l5w51/res.txt");
            String text = "none";
            if (res != null) {
                try (InputStream in = res.openStream()) {
                    text = new String(in.readAllBytes(), StandardCharsets.UTF_8);
                }
            }
            System.out.println("handler-" + round + "=" + line + " opened=" + (h.opened > 0)
                    + " resource=" + res + " text=" + text);
        }
    }

    public static void main(String[] args) throws Exception {
        // The application loader knows both helpers before any isolated loader
        // asks: a flat store would have them to answer with.
        System.out.print(Helper.v().isEmpty() ? "" : "");
        new Helper2();
        isolated(1);
        handler(1);
        isolated(2);
        handler(2);
    }
}
