// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L5 -- the static `ClassLoader` system
// resource doors, whose registered stand-ins (`getSystemResourceAsStream`,
// `getSystemResource`, `getSystemResources`) run over the JDK's concrete
// bodies (`docs/internal/fixed-bugs/interpreter-L5-bridges-still-run-over-jdk-bytecode-under-jdk-only-FIXED-20261007.md`,
// "Taken, not yielded": `getSystemResourceAsStream`). Each row is what the
// JDK body answers on HotSpot: the stand-in has to answer the same before it
// can be yielded, and must keep answering it while it stays.
//
// Rows (the first call, and again last):
//   stream-*    `getSystemResourceAsStream(<this class file>)`: non-null, the
//               first four bytes, the stream's class
//   url-*       `getSystemResource(<this class file>)`: protocol, path tail
//   urls-*      `getSystemResources(<this class file>)`: how many
//   missing-*   a name nobody has: `null` / an empty enumeration
//   null-*      a null name: the throwable's class and message
//   dir-*       a directory name (the probe's own output directory is the
//               class path entry, so `""` names it): non-null
//
// Run (no setup):
//   javac -d out L5W55SystemResourceRows.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W55SystemResourceRows
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   stream-1=cafebabe java.io.BufferedInputStream
//   url-1=file true
//   urls-1=1
//   missing-1=null null false
//   null-stream-1=java.lang.NullPointerException msg=null
//   null-url-1=java.lang.NullPointerException msg=null
//   null-urls-1=java.lang.NullPointerException msg=null
//   dir-1=file
//   stream-2=cafebabe java.io.BufferedInputStream
//   url-2=file true
//   urls-2=1
//   missing-2=null null false
//   null-stream-2=java.lang.NullPointerException msg=null
//   null-url-2=java.lang.NullPointerException msg=null
//   null-urls-2=java.lang.NullPointerException msg=null
//   dir-2=file
// CratonVM on the base (debug build of 63d2087232, Windows, both modes):
// `null-stream-N=null` and `null-urls-N=<an empty enumeration>` (under
// `--compatible` CratonVM's own `java.util.Enumeration$Impl`): fixed in wave
// 55. `stream-N=cafebabe java.io.ByteArrayInputStream` stayed, a known open
// difference:
// `docs/internal/fixed-bugs/interpreter-L5-classloader-resource-streams-are-bytearrayinputstreams-FIXED-20261021.md`;
// HotSpot's line since wave 56 (`CRATONVM_RESOURCE_STREAMS_OPEN_URL`, both
// modes, measured on a debug build; `=0` restores the old line).

import java.io.InputStream;
import java.net.URL;
import java.util.Enumeration;

public class L5W55SystemResourceRows {
    static final String SELF = "L5W55SystemResourceRows.class";

    static String attempt(java.util.concurrent.Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    static void rows(String tag) throws Exception {
        System.out.println("stream-" + tag + "=" + attempt(() -> {
            try (InputStream in = ClassLoader.getSystemResourceAsStream(SELF)) {
                if (in == null) {
                    return "null";
                }
                byte[] b = in.readNBytes(4);
                return String.format("%02x%02x%02x%02x", b[0], b[1], b[2], b[3]) + " "
                        + in.getClass().getName();
            }
        }));
        System.out.println("url-" + tag + "=" + attempt(() -> {
            URL u = ClassLoader.getSystemResource(SELF);
            return u == null ? "null" : u.getProtocol() + " " + u.getPath().endsWith("/" + SELF);
        }));
        System.out.println("urls-" + tag + "=" + attempt(() -> {
            Enumeration<URL> e = ClassLoader.getSystemResources(SELF);
            long n = 0;
            while (e.hasMoreElements()) {
                e.nextElement();
                n++;
            }
            return n;
        }));
        System.out.println("missing-" + tag + "=" + attempt(() ->
                ClassLoader.getSystemResourceAsStream("no/such/l5w55.txt") + " "
                        + ClassLoader.getSystemResource("no/such/l5w55.txt") + " "
                        + ClassLoader.getSystemResources("no/such/l5w55.txt").hasMoreElements()));
        System.out.println("null-stream-" + tag + "=" + attempt(() -> ClassLoader.getSystemResourceAsStream(null)));
        System.out.println("null-url-" + tag + "=" + attempt(() -> ClassLoader.getSystemResource(null)));
        System.out.println("null-urls-" + tag + "=" + attempt(() -> ClassLoader.getSystemResources(null)));
        System.out.println("dir-" + tag + "=" + attempt(() -> {
            URL u = ClassLoader.getSystemResource("");
            return u == null ? "null" : u.getProtocol();
        }));
    }

    public static void main(String[] args) throws Exception {
        rows("1");
        rows("2");
    }
}
