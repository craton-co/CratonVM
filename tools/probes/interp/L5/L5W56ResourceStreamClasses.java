// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L5 -- the class of a resource stream a
// built-in loader hands out for a `file:` class-path entry
// (`docs/internal/fixed-bugs/interpreter-L5-classloader-resource-streams-are-bytearrayinputstreams-FIXED-20261021.md`).
// HotSpot's bodies are `getResource(name)` / `getSystemResource(name)` then
// `url.openStream()`, so the stream is the URL connection's.
//
// Rows (the first call, and again last):
//   sys-*      `ClassLoader.getSystemResourceAsStream(<this class file>)`
//   app-*      the application loader's `getResourceAsStream(...)`
//   class-*    `L5W56ResourceStreamClasses.class.getResourceAsStream(...)`
//   slash-*    `getSystemResourceAsStream("/" + <this class file>)`: null
//   missing-*  a name nobody has: null, three doors
//   dir-*      `getSystemResourceAsStream("")` (the class-path directory)
// Each stream row: the first four bytes, the stream's class.
//
// Run (no setup; the class path must be a DIRECTORY, not a jar):
//   javac -d out L5W56ResourceStreamClasses.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W56ResourceStreamClasses
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   sys-1=cafebabe java.io.BufferedInputStream
//   app-1=cafebabe java.io.BufferedInputStream
//   class-1=cafebabe java.io.BufferedInputStream
//   slash-1=null
//   missing-1=null null null
//   dir-1=non-null java.io.ByteArrayInputStream
//   sys-2=cafebabe java.io.BufferedInputStream
//   app-2=cafebabe java.io.BufferedInputStream
//   class-2=cafebabe java.io.BufferedInputStream
//   slash-2=null
//   missing-2=null null null
//   dir-2=non-null java.io.ByteArrayInputStream
// CratonVM with `CRATONVM_RESOURCE_STREAMS_OPEN_URL=0` (debug build, Windows,
// measured): `sys-N=cafebabe java.io.ByteArrayInputStream` (both modes),
// `class-N=cafebabe java.io.ByteArrayInputStream` (`--compatible`),
// `slash-N=java.io.ByteArrayInputStream@...` (non-null) and `dir-N=null`
// (both modes). With the switch on, HotSpot's lines in both modes.
// `slash-urls-N` is the leading-slash `getResources` fix (not switched;
// before it, code read: `1 1`, the strip served the name).

import java.io.InputStream;
import java.util.concurrent.Callable;

public class L5W56ResourceStreamClasses {
    static final String SELF = "L5W56ResourceStreamClasses.class";

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
            return String.format("%02x%02x%02x%02x", b[0], b[1], b[2], b[3]) + " "
                    + in.getClass().getName();
        }
    }

    static long count(java.util.Enumeration<?> e) {
        long n = 0;
        while (e.hasMoreElements()) {
            e.nextElement();
            n++;
        }
        return n;
    }

    static void rows(String tag) {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        System.out.println("sys-" + tag + "=" + attempt(() -> head(ClassLoader.getSystemResourceAsStream(SELF))));
        System.out.println("app-" + tag + "=" + attempt(() -> head(app.getResourceAsStream(SELF))));
        System.out.println("class-" + tag + "=" + attempt(() ->
                head(L5W56ResourceStreamClasses.class.getResourceAsStream(SELF))));
        System.out.println("slash-" + tag + "=" + attempt(() -> ClassLoader.getSystemResourceAsStream("/" + SELF)));
        System.out.println("slash-urls-" + tag + "=" + attempt(() ->
                count(ClassLoader.getSystemResources("/" + SELF)) + " " + count(app.getResources("/" + SELF))));
        System.out.println("missing-" + tag + "=" + attempt(() ->
                ClassLoader.getSystemResourceAsStream("no/such/l5w56.txt") + " "
                        + app.getResourceAsStream("no/such/l5w56.txt") + " "
                        + L5W56ResourceStreamClasses.class.getResourceAsStream("/no/such/l5w56.txt")));
        System.out.println("dir-" + tag + "=" + attempt(() -> {
            InputStream in = ClassLoader.getSystemResourceAsStream("");
            if (in == null) {
                return "null";
            }
            try (in) {
                return "non-null " + in.getClass().getName();
            }
        }));
    }

    public static void main(String[] args) {
        rows("1");
        rows("2");
    }
}
