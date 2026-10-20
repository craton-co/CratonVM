// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.FileDescriptor;
import java.io.FileOutputStream;
import java.io.IOException;
import java.util.zip.Adler32;

/**
 * gcg g3/o (2026-10-01): door 2 of
 * {@code docs/internal/gc/gcg-g1o-reading-a-caught-oome-meets-non-collecting-doors-FIXED-20261001.md}.
 * A class's {@code java.lang.Class} mirror is asked for, for the first time,
 * by a native ({@code Object.getClass()}) while the heap is full and nothing
 * has collected yet. HotSpot creates every mirror at class load, so the call
 * allocates nothing there and always answers.
 *
 * <p>The heap is exhausted from C ({@code NewByteArray} held by global
 * references, as in {@code GcgO1JniOomeProbe}), which also drains whatever
 * the collector keeps back for callers that cannot fail. {@code Adler32} is a
 * bootstrap class instantiated (so initialized) before the fill and asked for
 * its mirror only after it.
 *
 * <p><b>gcg g6/oo: the oracle is re-derived.</b> The g3 version printed with
 * {@code System.out.println} right after the fill. HotSpot 25 Serial at
 * {@code -Xmx64m} printed NOTHING with it, and died with
 * {@code Exception: java.lang.OutOfMemoryError thrown from the
 * UncaughtExceptionHandler in thread "main"}: an allocation between the fill
 * and the first line failed on HotSpot too. The four "HotSpot lines" that
 * version documented were never produced by HotSpot. This version allocates
 * nothing from the fill until the heap is released and collected:
 * <ul>
 *   <li>every stdout line is a {@code byte[]} built before the fill and
 *       written through a {@code FileOutputStream} opened before it
 *       ({@code writeBytes} is a native that copies through a C buffer);</li>
 *   <li>every stage after the fill runs in a {@code catch (Throwable)}, so no
 *       error escapes {@code main}; a stage records a code, not a string;</li>
 *   <li>the class name is read only after {@code release()} and
 *       {@code System.gc()}.</li>
 *   <li>gci i26/oo2: that was not enough -- HotSpot still printed nothing
 *       (vi22-oo o3 rows, empty {@code .hs}). By reading, the first
 *       {@code catch (Throwable)} after the fill resolves {@code Throwable}
 *       in this class's constant pool through the app loader (a Java
 *       {@code loadClass} call, which allocates), and {@code release} is
 *       linked on its first call through {@code ClassLoader.findNative}
 *       (also Java). Both now happen before the fill: one caught
 *       {@code OutOfMemoryError} through {@code code}, one empty
 *       {@code release()}.</li>
 * </ul>
 * Stdout, one line per stage, in order (each a fixed choice; the oracle is
 * whatever HotSpot 25 Serial prints, see the g6 report):
 * <pre>
 *   fill=oome | fill=none | fill=other
 *   getClass=ok | getClass=oome | getClass=other
 *   release=ok | release=oome | release=other
 *   after-gc=ok | after-gc=oome | after-gc=other
 *   getClass-name=java.util.zip.Adler32 | getClass-name=none
 * </pre>
 * The expected HotSpot answer, by reading, is {@code fill=oome},
 * {@code getClass=ok}, {@code release=ok}, {@code after-gc=ok} and the name.
 * {@code getClass=oome} is the catchable error a non-collecting mirror door
 * throws instead (CratonVM's answer on a heap with no region left to claim).
 * A process abort prints nothing after the line it died in. Diagnostics on
 * stderr. Native half: {@code tools/probes/jni/GcgO3MirrorOomeProbe.c}.
 *
 * <p>{@code java -XX:+UseSerialGC -Xmx64m -cp <dir> GcgO3MirrorOomeProbe /tmp/libgcgo3mirroroome.so}
 */
public final class GcgO3MirrorOomeProbe {
    static native int fill(int chunk);

    static native int release();

    static long[] big;

    static final byte[][] FILL = {
        line("fill=oome"), line("fill=none"), line("fill=other")
    };
    static final byte[][] GET_CLASS = {
        line("getClass=ok"), line("getClass=oome"), line("getClass=other")
    };
    static final byte[][] RELEASE = {
        line("release=ok"), line("release=oome"), line("release=other")
    };
    static final byte[][] AFTER_GC = {
        line("after-gc=ok"), line("after-gc=oome"), line("after-gc=other")
    };
    static final byte[] NAME_NONE = line("getClass-name=none");

    static byte[] line(String s) {
        return (s + "\n").getBytes(java.nio.charset.StandardCharsets.US_ASCII);
    }

    /** 0 = ok, 1 = OutOfMemoryError, 2 = anything else. */
    static int code(Throwable t) {
        return t instanceof OutOfMemoryError ? 1 : 2;
    }

    static void emit(FileOutputStream out, byte[] b) {
        try {
            out.write(b);
        } catch (Throwable ignored) {
            // Nothing to report with: a failed write leaves its line missing.
        }
    }

    public static void main(String[] args) throws IOException {
        System.load(args[0]);
        final FileOutputStream out = new FileOutputStream(FileDescriptor.out);
        // Warm the write path and stderr before the heap is full.
        out.write(new byte[0]);
        System.err.print("");
        // gci i26/oo2: resolve, before the fill, everything the stages after
        // it touch for the first time. HotSpot resolves a catch type, an
        // `instanceof` target and a native of a class-path class lazily, on
        // first use, through the app loader (`loadClass` / `findNative` with a
        // Java String): on the drained heap that allocation fails, the error
        // replaces the one being caught, and HotSpot printed nothing (its
        // `.hs` was empty on every vi22-oo o3 row and in gcg g6).
        try {
            throw new OutOfMemoryError("warm");
        } catch (Throwable t) {
            emit(out, new byte[code(t) - 1]);
        }
        release();
        final Object fresh = new Adler32();
        int fillCode = 0;
        try {
            fill(64 * 1024);
            fillCode = 1; // no refusal: "none" (index 1 below)
        } catch (Throwable t) {
            fillCode = t instanceof OutOfMemoryError ? 0 : 2;
        }
        emit(out, FILL[fillCode]);
        // Still full: no collection has run since the refusal.
        Class<?> mirror = null;
        int getClassCode = 0;
        try {
            mirror = fresh.getClass();
        } catch (Throwable t) {
            getClassCode = code(t);
        }
        emit(out, GET_CLASS[getClassCode]);
        int releaseCode = 0;
        int held = -1;
        try {
            held = release();
        } catch (Throwable t) {
            releaseCode = code(t);
        }
        emit(out, RELEASE[releaseCode]);
        int afterCode = 0;
        try {
            System.gc();
            big = new long[1 << 20];
            big = null;
        } catch (Throwable t) {
            big = null;
            afterCode = code(t);
        }
        emit(out, AFTER_GC[afterCode]);
        try {
            if (mirror != null) {
                emit(out, line("getClass-name=" + mirror.getName()));
            } else {
                emit(out, NAME_NONE);
            }
            System.err.println("[probe] arrays held at the refusal=" + held);
        } catch (Throwable t) {
            // The heap was released and collected: an error here is itself the finding.
            emit(out, NAME_NONE);
        }
    }
}
