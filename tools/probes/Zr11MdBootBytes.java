// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr z11/md (2026-10-13): the Java-visible readers of a boot class's class
// file, for docs/internal/gc/zgc-zr8fm-class-metadata-per-class-cost-20261013.md.
//
// Under CRATONVM_BOOT_CLASS_BYTES_REFETCH (default ON) a class the boot loader
// defined from the runtime image keeps no cached class file; a reader re-reads
// it. Class.getAnnotatedSuperclass() / getAnnotatedInterfaces() is the reader
// a plain program reaches (CratonVM re-parses the defining class file for the
// type annotations on the extends / implements clauses). This probe asks it of
// every class it names and prints one checksum line, the same on HotSpot.
//
// Usage: Zr11MdBootBytes [n]     n = how many of the built-in names to use
// stdout: zr11md-bootbytes classes=<n> supers=<n> ifaces=<n> annotated=<n> crc=<hex> done
import java.lang.reflect.AnnotatedType;
import java.util.zip.CRC32;

public class Zr11MdBootBytes {
    static final String[] NAMES = {
        "java.lang.String", "java.lang.Integer", "java.lang.Thread", "java.lang.StringBuilder",
        "java.util.ArrayList", "java.util.HashMap", "java.util.LinkedHashMap", "java.util.TreeMap",
        "java.util.concurrent.ConcurrentHashMap", "java.util.concurrent.ThreadPoolExecutor",
        "java.util.AbstractList", "java.util.ArrayDeque", "java.util.PriorityQueue",
        "java.util.stream.ReferencePipeline", "java.io.BufferedReader", "java.io.PrintStream",
        "java.lang.invoke.MethodHandle", "java.lang.ref.WeakReference", "java.lang.Enum",
        "java.lang.Record", "java.util.Optional", "java.util.regex.Pattern",
        "java.time.LocalDate", "java.net.URI", "java.nio.ByteBuffer",
    };

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Math.min(Integer.parseInt(args[0]), NAMES.length) : NAMES.length;
        CRC32 crc = new CRC32();
        int supers = 0, ifaces = 0, annotated = 0;
        for (int i = 0; i < n; i++) {
            Class<?> c = Class.forName(NAMES[i]);
            AnnotatedType s = c.getAnnotatedSuperclass();
            if (s != null) {
                supers++;
                annotated += s.getAnnotations().length;
                crc.update(s.getType().getTypeName().getBytes("UTF-8"));
            }
            for (AnnotatedType t : c.getAnnotatedInterfaces()) {
                ifaces++;
                annotated += t.getAnnotations().length;
                crc.update(t.getType().getTypeName().getBytes("UTF-8"));
            }
        }
        System.out.println("zr11md-bootbytes classes=" + n + " supers=" + supers + " ifaces=" + ifaces
                + " annotated=" + annotated + " crc=" + Long.toHexString(crc.getValue()) + " done");
    }
}
