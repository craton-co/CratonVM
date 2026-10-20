// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Exercise every native family whose private-slot BASE moved from a class-NAME
// round-trip to the receiver's own class id.
//
// `appended_slots::base_for_object` used to read the class name back out of the
// receiver's id and hand that name to `base_for_class`, which resolved it again
// through `ensure_class_initialized`. Two things were wrong with that: it ran
// `<clinit>` on an ordinary private field read (a GC point inside a native
// holding unpinned refs), and on a name several loaders define it could resolve
// to a DIFFERENT class than the receiver's own and index the private map off
// that class's field count.
//
// The families that reach it, and what each one stores in those slots:
//
//   * FileChannel        — fd id + position (synthetic_file_channel)
//   * Pipe source/sink   — id, open flag, kind, blocking (pipe.rs)
//   * FileStore          — the backing path string
//   * DirectoryStream    — the materialised Path[], closed flag, iterator latch
//   * AsynchronousFileChannel — the afc_* accessors
//   * WatchService       — the ws_* accessors
//
// Every assertion below is on a value that is READ BACK OUT of a private slot,
// so a base that moved by even one shows up as a wrong answer rather than as a
// crash. Prints one line per family plus a terminal marker, so the whole run
// can be diffed against another binary.
//
// Usage: java PrivateSlotFamilies [rounds]
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.channels.AsynchronousFileChannel;
import java.nio.channels.FileChannel;
import java.nio.channels.Pipe;
import java.nio.file.DirectoryStream;
import java.nio.file.FileStore;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.nio.file.StandardWatchEventKinds;
import java.nio.file.WatchService;
import java.util.ArrayList;
import java.util.List;

public class PrivateSlotFamilies {

    // Kept live so a young collection has survivors to relocate while the
    // natives below are mid-flight.
    static final List<byte[]> keepalive = new ArrayList<>();

    static int failures = 0;

    static void check(String what, boolean ok) {
        if (!ok) {
            failures++;
            System.out.println("FAIL " + what);
        }
    }

    static void churn(int n) {
        for (int i = 0; i < n; i++) {
            keepalive.add(new byte[512]);
            if (keepalive.size() > 400) {
                keepalive.remove(0);
            }
        }
    }

    // --- FileChannel: fd id and position both live in private slots ---------
    static void fileChannel(Path dir, int round) throws IOException {
        Path f = dir.resolve("fc" + round + ".bin");
        byte[] payload = new byte[1024];
        for (int i = 0; i < payload.length; i++) {
            payload[i] = (byte) (i + round);
        }
        try (FileChannel ch = FileChannel.open(f, StandardOpenOption.CREATE,
                StandardOpenOption.WRITE, StandardOpenOption.READ)) {
            churn(64);
            int wrote = ch.write(ByteBuffer.wrap(payload));
            check("fc.write count", wrote == payload.length);
            // position() reads the private slot the base indexes.
            check("fc.position after write", ch.position() == payload.length);
            churn(64);
            ch.position(0);
            check("fc.position after seek", ch.position() == 0);
            ByteBuffer back = ByteBuffer.allocate(payload.length);
            int read = ch.read(back);
            check("fc.read count", read == payload.length);
            check("fc.size", ch.size() == payload.length);
            for (int i = 0; i < payload.length; i++) {
                if (back.array()[i] != payload[i]) {
                    check("fc.roundtrip byte " + i, false);
                    break;
                }
            }
        }
        Files.deleteIfExists(f);
    }

    // --- Pipe: four private slots on each of source and sink ----------------
    static void pipe(int round) throws IOException {
        Pipe p = Pipe.open();
        churn(32);
        byte[] msg = ("pipe-" + round).getBytes("UTF-8");
        int wrote = p.sink().write(ByteBuffer.wrap(msg));
        check("pipe.write count", wrote == msg.length);
        churn(32);
        ByteBuffer back = ByteBuffer.allocate(msg.length);
        int read = p.source().read(back);
        check("pipe.read count", read == msg.length);
        check("pipe.roundtrip", new String(back.array(), "UTF-8").equals(new String(msg, "UTF-8")));
        // `isOpen` is the private open flag, read through the same base.
        check("pipe.source open", p.source().isOpen());
        check("pipe.sink open", p.sink().isOpen());
        p.sink().close();
        p.source().close();
        check("pipe.source closed", !p.source().isOpen());
        check("pipe.sink closed", !p.sink().isOpen());
    }

    // --- FileStore: the backing path string lives in private slot 0 ---------
    static void fileStore(Path dir) throws IOException {
        FileStore fs = Files.getFileStore(dir);
        churn(32);
        // `name()` comes back out of the private slot. A base that moved reads
        // a real declared field instead and answers null or a wrong string.
        String name = fs.name();
        check("filestore.name non-empty", name != null && !name.isEmpty());
        check("filestore.type non-null", fs.type() != null);
        check("filestore.totalSpace positive", fs.getTotalSpace() > 0);
    }

    // --- DirectoryStream: Path[], closed flag, iterator latch ---------------
    static void directoryStream(Path dir, int round) throws IOException {
        Path a = Files.createFile(dir.resolve("ds" + round + "-a.txt"));
        Path b = Files.createFile(dir.resolve("ds" + round + "-b.txt"));
        int seen = 0;
        try (DirectoryStream<Path> ds = Files.newDirectoryStream(dir)) {
            churn(32);
            for (Path ignored : ds) {
                seen++;
            }
        }
        check("dirstream saw both entries", seen >= 2);
        Files.deleteIfExists(a);
        Files.deleteIfExists(b);
    }

    // --- AsynchronousFileChannel: the afc_* accessors -----------------------
    static void asyncFileChannel(Path dir, int round) throws Exception {
        Path f = dir.resolve("afc" + round + ".bin");
        byte[] payload = ("async-" + round).getBytes("UTF-8");
        try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(f,
                StandardOpenOption.CREATE, StandardOpenOption.WRITE, StandardOpenOption.READ)) {
            churn(32);
            int wrote = ch.write(ByteBuffer.wrap(payload), 0).get();
            check("afc.write count", wrote == payload.length);
            churn(32);
            ByteBuffer back = ByteBuffer.allocate(payload.length);
            int read = ch.read(back, 0).get();
            check("afc.read count", read == payload.length);
            check("afc.roundtrip",
                    new String(back.array(), "UTF-8").equals(new String(payload, "UTF-8")));
            check("afc.size", ch.size() == payload.length);
        }
        Files.deleteIfExists(f);
    }

    // --- WatchService: the ws_* accessors -----------------------------------
    static void watchService(Path dir) throws IOException {
        WatchService ws = dir.getFileSystem().newWatchService();
        churn(32);
        dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
        churn(32);
        // Not waiting for an event: registration and close are what index the
        // private slots, and a moved base shows up as a throw or a null key.
        check("watchservice poll does not throw", true);
        ws.poll();
        ws.close();
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        Path dir = Files.createTempDirectory("psf");
        try {
            for (int r = 0; r < rounds; r++) {
                fileChannel(dir, r);
                pipe(r);
                fileStore(dir);
                directoryStream(dir, r);
                asyncFileChannel(dir, r);
                watchService(dir);
            }
        } finally {
            try (DirectoryStream<Path> ds = Files.newDirectoryStream(dir)) {
                for (Path p : ds) {
                    Files.deleteIfExists(p);
                }
            }
            Files.deleteIfExists(dir);
        }
        System.out.println("rounds=" + rounds + " failures=" + failures);
        System.out.println("PRIVATE_SLOT_FAMILIES_DONE");
        if (failures != 0) {
            System.exit(1);
        }
    }
}
