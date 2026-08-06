// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.File;
import java.io.FileOutputStream;
import java.io.RandomAccessFile;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.CharBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/**
 * Which concrete class does each buffer factory hand back, and does an
 * <em>unoverridden</em> method work on the result?
 *
 * <p>Written for the residual left open by
 * {@code docs/known-issues/nio/buffer-address-indexed-slot-aliasing.md}: CratonVM
 * allocates buffers stamped with the ABSTRACT {@code java.nio.ByteBuffer} /
 * {@code java.nio.CharBuffer} at roughly eighteen sites. The workaround has been
 * to register a native for each abstract method those receivers might reach —
 * which holds only for the methods someone thought of. Any real-JDK method
 * without an override resolves to its abstract declaration:
 *
 * <pre>
 *   AbstractMethodError: method java/nio/ByteBuffer.slice()Ljava/nio/ByteBuffer;
 *                        has no Code attribute
 * </pre>
 *
 * <p>So the probe does two things per factory. It prints the buffer's concrete
 * class — {@code kind=} — which is the property the fix changes, and it calls a
 * spread of methods, printing a one-word verdict for each. A verdict is a shape,
 * never a value, except where the value IS the contract ({@code arrayOffset},
 * {@code remaining}) — buffer contents and addresses differ between runs and
 * platforms, and a probe that diffs those is measuring the host.
 *
 * <p>{@code address} is read reflectively and reported only as
 * {@code base}/{@code base+off}/{@code other}: the raw number is
 * {@code ARRAY_*_BASE_OFFSET}-dependent, but "does a slice carry a LARGER
 * address than its parent" is exactly the invariant the earlier half of this
 * doc got wrong by hardcoding 16. Needs
 * {@code --add-opens java.base/java.nio=ALL-UNNAMED}; without it the section
 * prints {@code address=<no-access>} in every arm and still diffs clean.
 */
public class NioBufferStampProbe {

    public static void main(String[] args) throws Exception {
        heapByteBuffer();
        wrappedByteBuffer();
        directByteBuffer();
        charBufferViews();
        heapCharBuffer();
        fileChannelTransfer();
    }

    // ------------------------------------------------------------- factories

    static void heapByteBuffer() {
        ByteBuffer b = ByteBuffer.allocate(64);
        System.out.println("allocate " + kind(b) + " " + exercise(b) + " " + addressShape(b));
    }

    static void wrappedByteBuffer() {
        byte[] backing = new byte[64];
        ByteBuffer b = ByteBuffer.wrap(backing, 8, 32);
        System.out.println("wrap " + kind(b) + " arrayOffset=" + safeArrayOffset(b)
                + " " + exercise(b) + " " + addressShape(b));
        // `wrap(a, off, len)` moves POSITION, not arrayOffset — only `slice()`
        // produces a buffer whose `offset` is non-zero, and therefore the only
        // one that can tell "preserved the real address" apart from "wrote the
        // constant 16". That distinction is what the CharBuffer half of this
        // doc originally got wrong.
        String sliced;
        try {
            ByteBuffer s = b.slice();
            sliced = kind(s) + " arrayOffset=" + safeArrayOffset(s) + " " + addressShape(s);
        } catch (Throwable t) {
            sliced = verdict(t);
        }
        System.out.println("wrapSlice " + sliced);
    }

    static void directByteBuffer() {
        ByteBuffer b = ByteBuffer.allocateDirect(64);
        System.out.println("allocateDirect " + kind(b) + " direct=" + b.isDirect()
                + " hasArray=" + b.hasArray() + " " + exercise(b));
    }

    static void charBufferViews() {
        ByteBuffer b = ByteBuffer.allocate(64);
        CharBuffer c;
        try {
            c = b.asCharBuffer();
        } catch (Throwable t) {
            System.out.println("asCharBuffer " + verdict(t));
            return;
        }
        System.out.println("asCharBuffer " + kind(c) + " " + exerciseChar(c));
    }

    static void heapCharBuffer() {
        CharBuffer c = CharBuffer.allocate(64);
        CharBuffer w = CharBuffer.wrap(new char[64], 8, 32);
        System.out.println("charAllocate " + kind(c) + " " + exerciseChar(c) + " " + addressShape(c));
        System.out.println("charWrap " + kind(w) + " arrayOffset=" + safeArrayOffset(w)
                + " " + exerciseChar(w) + " " + addressShape(w));
        String sliced;
        try {
            CharBuffer s = w.slice();
            sliced = kind(s) + " arrayOffset=" + safeArrayOffset(s) + " " + addressShape(s);
        } catch (Throwable t) {
            sliced = verdict(t);
        }
        System.out.println("charWrapSlice " + sliced);
    }

    /**
     * `FileChannel.transferTo`/`transferFrom` build their own intermediate
     * ByteBuffer inside the VM and hand it to the OTHER channel's
     * `write`/`read`. That receiver is real JDK code, so the intermediate has to
     * be a real buffer — and the byte count it reports is the whole contract.
     */
    static void fileChannelTransfer() throws Exception {
        Path dir = Files.createTempDirectory("nio-buf-stamp");
        Path src = dir.resolve("src.bin");
        Path dst = dir.resolve("dst.bin");
        byte[] payload = new byte[4096];
        for (int i = 0; i < payload.length; i++) {
            payload[i] = (byte) i;
        }
        Files.write(src, payload);
        Files.write(dst, new byte[0]);

        String to;
        try (FileChannel in = FileChannel.open(src, StandardOpenOption.READ);
             FileChannel out = FileChannel.open(dst, StandardOpenOption.WRITE)) {
            long n = in.transferTo(0, payload.length, out);
            to = "bytes=" + n;
        } catch (Throwable t) {
            to = verdict(t);
        }
        boolean sameTo;
        try {
            sameTo = java.util.Arrays.equals(Files.readAllBytes(dst), payload);
        } catch (Throwable t) {
            sameTo = false;
        }

        Path dst2 = dir.resolve("dst2.bin");
        Files.write(dst2, new byte[0]);
        String from;
        try (FileChannel in = FileChannel.open(src, StandardOpenOption.READ);
             FileChannel out = FileChannel.open(dst2, StandardOpenOption.WRITE)) {
            long n = out.transferFrom(in, 0, payload.length);
            from = "bytes=" + n;
        } catch (Throwable t) {
            from = verdict(t);
        }
        boolean sameFrom;
        try {
            sameFrom = java.util.Arrays.equals(Files.readAllBytes(dst2), payload);
        } catch (Throwable t) {
            sameFrom = false;
        }

        System.out.println("transferTo " + to + " identical=" + sameTo
                + " transferFrom " + from + " identical=" + sameFrom);

        deleteQuietly(dst2);
        deleteQuietly(dst);
        deleteQuietly(src);
        deleteQuietly(dir);
    }

    // -------------------------------------------------------------- exercise

    /**
     * Call the methods a caller reaches without thinking about them. Every one
     * of these is declared abstract on `ByteBuffer` or inherited from `Buffer`,
     * so a receiver stamped with the abstract class needs a native for each —
     * and only the ones someone remembered exist.
     */
    static String exercise(ByteBuffer b) {
        StringBuilder sb = new StringBuilder();
        sb.append("slice=").append(call(() -> kind(b.slice())));
        sb.append(" dup=").append(call(() -> kind(b.duplicate())));
        sb.append(" ro=").append(call(() -> kind(b.asReadOnlyBuffer())));
        sb.append(" order=").append(call(() -> String.valueOf(b.order() == ByteOrder.BIG_ENDIAN)));
        sb.append(" rem=").append(call(() -> String.valueOf(b.remaining())));
        sb.append(" compact=").append(call(() -> { b.duplicate().compact(); return "ok"; }));
        sb.append(" put=").append(call(() -> { b.duplicate().put(0, (byte) 7); return "ok"; }));
        sb.append(" getInt=").append(call(() -> { b.duplicate().getInt(0); return "ok"; }));
        sb.append(" eq=").append(call(() -> String.valueOf(b.equals(b.duplicate()))));
        sb.append(" cmp=").append(call(() -> String.valueOf(b.compareTo(b.duplicate()) == 0)));
        sb.append(" hash=").append(call(() -> String.valueOf(b.hashCode() == b.duplicate().hashCode())));
        sb.append(" bulk=").append(call(() -> {
            ByteBuffer d = b.duplicate();
            ByteBuffer t = ByteBuffer.allocate(d.remaining());
            t.put(d);
            return "ok";
        }));
        return sb.toString();
    }

    static String exerciseChar(CharBuffer c) {
        StringBuilder sb = new StringBuilder();
        sb.append("slice=").append(call(() -> kind(c.slice())));
        sb.append(" dup=").append(call(() -> kind(c.duplicate())));
        sb.append(" ro=").append(call(() -> kind(c.asReadOnlyBuffer())));
        sb.append(" rem=").append(call(() -> String.valueOf(c.remaining())));
        sb.append(" sub=").append(call(() -> kind(c.duplicate().subSequence(0, 4))));
        sb.append(" put=").append(call(() -> { c.duplicate().put(0, 'x'); return "ok"; }));
        sb.append(" str=").append(call(() -> String.valueOf(c.duplicate().toString().length())));
        sb.append(" bulk=").append(call(() -> {
            CharBuffer d = c.duplicate();
            CharBuffer t = CharBuffer.allocate(d.remaining());
            t.put(d);
            return "ok";
        }));
        return sb.toString();
    }

    // --------------------------------------------------------------- helpers

    interface Body {
        String run() throws Throwable;
    }

    /** A one-word verdict: what it returned, or what class of failure it was. */
    static String call(Body body) {
        try {
            return body.run();
        } catch (Throwable t) {
            return verdict(t);
        }
    }

    static String verdict(Throwable t) {
        Throwable c = t;
        while (c.getCause() != null && c.getCause() != c) {
            c = c.getCause();
        }
        return "throw-" + c.getClass().getSimpleName();
    }

    /** The buffer's concrete class, simple name only — the property under test. */
    static String kind(Object o) {
        return o == null ? "kind=null" : "kind=" + o.getClass().getSimpleName();
    }

    static String safeArrayOffset(java.nio.Buffer b) {
        try {
            return String.valueOf(b.getClass().getMethod("arrayOffset").invoke(b));
        } catch (Throwable t) {
            return verdict(t);
        }
    }

    /**
     * `address` as a SHAPE. A heap buffer's address is
     * `ARRAY_<T>_BASE_OFFSET + offset * scale`, so the number varies with the
     * JDK's array header — but "a wrapped-with-offset buffer carries a larger
     * address than a zero-offset one" does not, and that is the invariant a
     * hardcoded constant breaks.
     */
    static String addressShape(java.nio.Buffer b) {
        try {
            java.lang.reflect.Field f = java.nio.Buffer.class.getDeclaredField("address");
            f.setAccessible(true);
            long addr = f.getLong(b);
            int off = Integer.parseInt(safeArrayOffset(b));
            if (addr <= 0) {
                return "address=nonpositive";
            }
            return off == 0 ? "address=base" : "address=base+off";
        } catch (Throwable t) {
            return "address=<no-access>";
        }
    }

    static void deleteQuietly(Path p) {
        try {
            Files.deleteIfExists(p);
        } catch (Throwable ignored) {
            // Best effort; the temp dir is the OS's problem after this.
        }
    }
}
