// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 52, lane L5 -- the positive control of the
// `jdk-url-class-loaders` HotSpot-alignment row (`--compatible` over a real
// JDK runs the JDK's own `URLClassLoader` / `URLClassPath` bodies, as
// `--jdk-only` does since the wave-24 fat-jar retirement; item 6 of
// `docs/internal/fixed-bugs/interpreter-L5-compatible-gates-that-still-differ-after-the-alignment-table-FIXED-20261007.md`).
// Every row is first (a cold VM) and again last, on fresh loaders:
//
//   app-parent  `new URLClassLoader(urls)` (application parent) over
//               `l5w52mem:/root/`, a URL whose `URLStreamHandler` is the
//               application's and serves a generated `l5w52.Gen` (on no class
//               path) from memory: the class's loader, its `CodeSource`
//               location, a repeated `loadClass`, and whether the handler was
//               opened. The `--compatible` stand-in read the URL back as a
//               filesystem path: `ClassNotFoundException`.
//   resources   `getResource` / `getResources` of a resource only the handler
//               has, through an anonymous `URLClassLoader` subclass (a user
//               loader: `ClassLoader.getResource`'s body, then the real
//               `findResource`), with the text read back.
//   urls        `getURLs()` of that loader.
//   boot-res    a null-parent `URLClassLoader`'s `getResource` of
//               `java/lang/Object.class` (the JDK's `ClassLoader.getResource`
//               asks `BootLoader.findResource`): its protocol.
//
// Run (no setup):
//   javac -d out L5W52CompatUrlClassLoaderBodies.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W52CompatUrlClassLoaderBodies
//
// The row was inert on Windows until the wave's orchestrator follow-up fixed
// `--compatible`'s `java.io.File` drive prefix
// (`docs/internal/fixed-bugs/interpreter-L5-compatible-file-keeps-a-slash-before-a-windows-drive-FIXED-20261016.md`);
// since then a Windows `--compatible` run prints the expected lines too.
//
// Positive control (`--compatible`): `CRATONVM_COMPAT_ALIGN=-jdk-url-class-loaders`
// prints the stand-in's rows (`app-parent-N=load java.lang.ClassNotFoundException
// opened=false`, `resources-N=... resource=null ...`); with the row on,
// `CRATONVM_DBG=access` makes the exit line `[ACCESS-DBG] ALIGNMENT census:
// ...` count `jdk-url-class-loaders=<n>` with n > 0.
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   app-parent-1=loader=true location=l5w52mem:/root/ again=true opened=true
//   resources-1=resource=l5w52mem:/root/l5w52/res.txt all=1 text=hello
//   urls-1=[l5w52mem:/root/]
//   boot-res-1=jrt
//   app-parent-2=loader=true location=l5w52mem:/root/ again=true opened=true
//   resources-2=resource=l5w52mem:/root/l5w52/res.txt all=1 text=hello
//   urls-2=[l5w52mem:/root/]
//   boot-res-2=jrt

import java.io.ByteArrayInputStream;
import java.io.FileNotFoundException;
import java.io.InputStream;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.net.URL;
import java.net.URLClassLoader;
import java.net.URLConnection;
import java.net.URLStreamHandler;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashMap;
import java.util.Map;

public class L5W52CompatUrlClassLoaderBodies {
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
                public InputStream getInputStream() throws java.io.IOException {
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

    static byte[] gen() {
        return ClassFile.of().build(ClassDesc.of("l5w52.Gen"), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object));
    }

    @SuppressWarnings("deprecation")
    static URL base(MemHandler h) throws Exception {
        h.files.put("/root/l5w52/Gen.class", gen());
        h.files.put("/root/l5w52/res.txt", "hello".getBytes(StandardCharsets.UTF_8));
        return new URL(null, "l5w52mem:/root/", h);
    }

    static void appParent(int round) throws Exception {
        MemHandler h = new MemHandler();
        URL base = base(h);
        String line;
        try (URLClassLoader u = new URLClassLoader(new URL[] {base})) {
            try {
                Class<?> c = u.loadClass("l5w52.Gen");
                URL loc = c.getProtectionDomain().getCodeSource() == null
                        ? null
                        : c.getProtectionDomain().getCodeSource().getLocation();
                line = "loader=" + (c.getClassLoader() == u) + " location=" + loc
                        + " again=" + (u.loadClass("l5w52.Gen") == c);
            } catch (Throwable t) {
                line = "load " + t.getClass().getName();
            }
        }
        System.out.println("app-parent-" + round + "=" + line + " opened=" + (h.opened > 0));
    }

    static void resources(int round) throws Exception {
        MemHandler h = new MemHandler();
        URL base = base(h);
        try (URLClassLoader u = new URLClassLoader(new URL[] {base}, null) {}) {
            URL res = u.getResource("l5w52/res.txt");
            int all = Collections.list(u.getResources("l5w52/res.txt")).size();
            String text = "none";
            if (res != null) {
                try (InputStream in = res.openStream()) {
                    text = new String(in.readAllBytes(), StandardCharsets.UTF_8);
                }
            }
            System.out.println("resources-" + round + "=resource=" + res + " all=" + all + " text=" + text);
            System.out.println("urls-" + round + "=" + Arrays.toString(u.getURLs()));
        }
    }

    static void bootRes(int round) throws Exception {
        try (URLClassLoader u = new URLClassLoader(new URL[0], null)) {
            URL r = u.getResource("java/lang/Object.class");
            System.out.println("boot-res-" + round + "=" + (r == null ? "null" : r.getProtocol()));
        }
    }

    public static void main(String[] args) throws Exception {
        for (int round = 1; round <= 2; round++) {
            appParent(round);
            resources(round);
            bootRes(round);
        }
    }
}
