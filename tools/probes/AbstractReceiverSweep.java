// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// The assertion that needs no oracle: no factory in the JDK may hand back an
// instance whose runtime class is ABSTRACT or an INTERFACE.
//
// JVMS §6.5 makes `new` on either an `InstantiationError`, so such a receiver is
// one no bytecode in any image could have produced. That makes every DEFECT
// line below self-evident — it does not have to be compared against HotSpot to
// be wrong, which is what makes this sweep worth running on a host with no
// oracle at all.
//
// `native-api/src/instantiable.rs` describes this probe as `probes/W4Abstract.java`
// and lists four sites it found. The file was never in the tree; this is it,
// widened to the families that have since been recorded — java.nio.channels,
// java.nio.file, java.nio.fs, and the FFM segment carriers, which the
// definition-of-done screen reached from the other direction:
// `cratonvm/internal/foreign/MemorySegmentImpl` is refused under `--jdk-only`
// and the fallback allocates against `java.lang.foreign.MemorySegment` — the
// INTERFACE.
//
// It also demonstrates the hole that let that hide. `--jdk-only`'s predicate is
// `compatibility_classes: 0`, which counts classes MINTED; an instance
// allocated against a real class that happens to be an interface is invisible
// to it. Both statements can be true of one run, and were.
//
//   javac -d out AbstractReceiverSweep.java
//   java              -cp out AbstractReceiverSweep      # oracle
//   cratonvm --jdk-only -cp out AbstractReceiverSweep    # and the VM
//
// Every printed value is chosen by the program: a class name, and two booleans
// read from `Class.getModifiers()`. No addresses, no identity hashes, no
// iteration order.

import java.io.File;
import java.io.FileOutputStream;
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.reflect.Modifier;
import java.nio.ByteBuffer;
import java.nio.channels.DatagramChannel;
import java.nio.channels.Pipe;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.charset.Charset;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;

public final class AbstractReceiverSweep {

    private static int rows = 0;
    private static int defects = 0;

    public static void main(String[] args) throws Exception {
        Path tmp = Files.createTempDirectory("ars");

        // --- java.nio.channels ------------------------------------------
        one("Pipe.open()", () -> Pipe.open());
        one("Pipe.open().source()", () -> Pipe.open().source());
        one("Pipe.open().sink()", () -> Pipe.open().sink());
        one("DatagramChannel.open()", () -> DatagramChannel.open());
        one("SocketChannel.open()", () -> SocketChannel.open());
        one("ServerSocketChannel.open()", () -> ServerSocketChannel.open());
        one("Selector.open()", () -> Selector.open());
        one("Selector.open().provider()", () -> Selector.open().provider());
        one("FileOutputStream.getChannel()", () -> {
            File f = new File(tmp.toFile(), "c.bin");
            try (FileOutputStream o = new FileOutputStream(f)) {
                return o.getChannel();
            }
        });

        // --- java.nio.file / java.nio.fs --------------------------------
        one("FileSystems.getDefault()", () -> FileSystems.getDefault());
        one("FileSystems.getDefault().provider()", () -> FileSystems.getDefault().provider());
        one("FileSystems.getDefault().newWatchService()",
                () -> FileSystems.getDefault().newWatchService());
        one("Path.of(..)", () -> Path.of("a", "b"));
        one("Files.newDirectoryStream(dir)", () -> Files.newDirectoryStream(tmp));
        one("Files.getFileStore(dir)", () -> Files.getFileStore(tmp));
        one("Path.getFileSystem()", () -> tmp.getFileSystem());

        // --- java.lang.foreign ------------------------------------------
        // The definition-of-done screen's remaining fabrication request:
        // `cratonvm/internal/foreign/MemorySegmentImpl` is refused under
        // `--jdk-only` and `alloc_segment_carrier`'s fallback allocates
        // against the MemorySegment INTERFACE.
        one("Arena.ofConfined().allocate(16)", () -> Arena.ofConfined().allocate(16));
        one("Arena.global().allocate(8)", () -> Arena.global().allocate(8));
        one("Arena.ofConfined()", () -> Arena.ofConfined());
        one("MemorySegment.ofArray(byte[16])", () -> MemorySegment.ofArray(new byte[16]));
        one("MemorySegment.ofArray(int[4])", () -> MemorySegment.ofArray(new int[4]));
        one("MemorySegment.NULL", () -> MemorySegment.NULL);
        one("Arena.ofConfined().allocate(16).asSlice(4)",
                () -> Arena.ofConfined().allocate(16).asSlice(4));
        one("ByteBuffer.allocateDirect(8)", () -> ByteBuffer.allocateDirect(8));
        one("MemorySegment.ofBuffer(directBuffer)",
                () -> MemorySegment.ofBuffer(ByteBuffer.allocateDirect(8)));

        // --- assorted platform singletons -------------------------------
        one("Charset.defaultCharset()", () -> Charset.defaultCharset());
        one("ClassLoader.getSystemClassLoader()", () -> ClassLoader.getSystemClassLoader());
        one("ProcessHandle.current()", () -> ProcessHandle.current());
        one("Thread.currentThread()", () -> Thread.currentThread());

        System.out.println("DOD ROWS " + rows + " DEFECTS " + defects);
        System.out.println("DOD RESULT " + (defects == 0 ? "OK" : "DEFECTS=" + defects));
    }

    private interface Factory {
        Object make() throws Exception;
    }

    /**
     * One factory. Prints the runtime class and the verdict.
     *
     * <p>The VERDICT half needs no oracle. The NAME half is comparable against
     * HotSpot and is what says whether the concrete class is also the right
     * one — a factory can answer a perfectly instantiable class that is still
     * not the one the JDK would have built.
     */
    private static void one(String label, Factory f) {
        rows++;
        Object o;
        try {
            o = f.make();
        } catch (Throwable t) {
            // A refusal is a result, not a hole: print it and move on. It is
            // never counted as an abstract-receiver defect, which is a claim
            // about an object that exists.
            System.out.println("DOD SITE " + label + " => THREW "
                    + t.getClass().getName() + ": " + t.getMessage());
            return;
        }
        if (o == null) {
            System.out.println("DOD SITE " + label + " => null");
            return;
        }
        Class<?> c = o.getClass();
        boolean iface = c.isInterface();
        boolean abs = Modifier.isAbstract(c.getModifiers());
        boolean bad = iface || abs;
        if (bad) {
            defects++;
        }
        System.out.println("DOD SITE " + label + " => " + c.getName()
                + " interface=" + iface + " abstract=" + abs
                + (bad ? "   <- DEFECT: no `new` could have produced this" : ""));
    }
}
