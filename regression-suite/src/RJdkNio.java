import java.io.FileInputStream;
import java.io.IOException;
import java.io.RandomAccessFile;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.MappedByteBuffer;
import java.nio.channels.AsynchronousCloseException;
import java.nio.channels.ClosedChannelException;
import java.nio.channels.FileChannel;
import java.nio.channels.FileLock;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.DirectoryStream;
import java.nio.file.FileAlreadyExistsException;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.NoSuchFileException;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.StandardOpenOption;
import java.nio.file.attribute.BasicFileAttributes;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

/**
 * JDK-only corpus: files and NIO -- random access, mapping, channels,
 * selectors, asynchronous close.
 *
 * "NIO, files, networking" is a named P1 blocker: file descriptors, mapping,
 * selectors, asynchronous close and the Windows/Unix provider split are a mix
 * of real bytecode and bridges today.
 *
 * Determinism: everything happens under a fresh temp directory whose ABSOLUTE
 * PATH IS NEVER PRINTED; only file NAMES, sizes and contents chosen by this
 * vector are emitted. Directory listings are sorted.
 */
public class RJdkNio {
    static final long T = 30;
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static void deleteTree(Path root) {
        try {
            if (!Files.exists(root)) {
                return;
            }
            List<Path> all = new ArrayList<>();
            try (java.util.stream.Stream<Path> s = Files.walk(root)) {
                s.forEach(all::add);
            }
            Collections.reverse(all);
            for (Path p : all) {
                try {
                    Files.deleteIfExists(p);
                } catch (IOException ignored) {
                    // A mapped file may still be pinned on Windows; leaving it
                    // behind in the OS temp dir is harmless for the assertions.
                }
            }
        } catch (IOException ignored) {
            // Cleanup is best-effort and never part of the assertions.
        }
    }

    static void filesApi(Path dir) throws Exception {
        Path f = dir.resolve("plain.txt");
        Files.write(f, "line1\nline2\n".getBytes(StandardCharsets.UTF_8));
        check(Files.exists(f) && Files.isRegularFile(f), "file created");
        check(Files.size(f) == 12, "size: " + Files.size(f));
        check(Files.readAllLines(f, StandardCharsets.UTF_8).equals(Arrays.asList("line1", "line2")),
                "readAllLines");
        check(new String(Files.readAllBytes(f), StandardCharsets.UTF_8).equals("line1\nline2\n"),
                "readAllBytes");

        BasicFileAttributes attrs = Files.readAttributes(f, BasicFileAttributes.class);
        check(attrs.isRegularFile() && !attrs.isDirectory(), "attributes kind");
        check(attrs.size() == 12, "attribute size");
        check(attrs.lastModifiedTime() != null, "lastModifiedTime present");

        Path copy = dir.resolve("copy.txt");
        Files.copy(f, copy);
        check(Files.size(copy) == 12, "copy size");
        boolean threw = false;
        try {
            Files.copy(f, copy);
        } catch (FileAlreadyExistsException expected) {
            threw = true;
        }
        check(threw, "copy onto an existing file must throw FileAlreadyExistsException");
        Files.copy(f, copy, StandardCopyOption.REPLACE_EXISTING);

        Path moved = dir.resolve("moved.txt");
        Files.move(copy, moved);
        check(!Files.exists(copy) && Files.exists(moved), "move");

        Path sub = Files.createDirectories(dir.resolve("a/b/c"));
        check(Files.isDirectory(sub), "createDirectories");
        Files.write(sub.resolve("deep.txt"), new byte[] { 1, 2, 3 });

        List<String> names = new ArrayList<>();
        try (DirectoryStream<Path> ds = Files.newDirectoryStream(dir)) {
            for (Path p : ds) {
                names.add(p.getFileName().toString());
            }
        }
        Collections.sort(names);
        check(names.equals(Arrays.asList("a", "moved.txt", "plain.txt")),
                "directory listing: " + names);

        threw = false;
        try {
            Files.readAllBytes(dir.resolve("absent.txt"));
        } catch (NoSuchFileException expected) {
            threw = true;
        }
        check(threw, "reading a missing file must throw NoSuchFileException");
        check(!Files.deleteIfExists(dir.resolve("absent.txt")), "deleteIfExists on a miss");

        // Files.createFile is CREATE_NEW. It is the atomic create-if-absent
        // primitive of java.nio.file, so callers use it AS a lock rather than
        // merely to make a file, and a second call on the same path must fail
        // with FileAlreadyExistsException -- and must not touch the bytes that
        // are already there. CratonVM implemented it with an O_CREAT|O_TRUNC
        // open, which got both halves wrong at once: it reported success and
        // emptied the file. H2 FilePathDisk.createFile catches the exception to
        // answer "another process holds this lock", so two FileLocks both
        // believed they had taken the database lock (TestFileLock.testSimple
        // failed with ERROR_OPENING_DATABASE_1 where it asserts
        // DATABASE_ALREADY_OPEN_1). Runs after the directory listing above on
        // purpose, and cleans up after itself, so the CK line stays stable.
        Path fresh = dir.resolve("createnew.txt");
        Files.createFile(fresh);
        check(Files.exists(fresh) && Files.size(fresh) == 0, "createFile makes an empty file");
        Files.write(fresh, new byte[] { 7, 7, 7 });
        threw = false;
        try {
            Files.createFile(fresh);
        } catch (FileAlreadyExistsException expected) {
            threw = true;
        }
        check(threw, "createFile on an existing path must throw FileAlreadyExistsException");
        check(Files.size(fresh) == 3, "a refused createFile must not truncate the existing file");
        Files.delete(fresh);

        // Path arithmetic is pure string work and must be exact.
        Path rel = Path.of("a", "b", "c.txt");
        check(rel.getNameCount() == 3, "getNameCount");
        check(rel.getFileName().toString().equals("c.txt"), "getFileName");
        check(rel.getParent().toString().replace('\\', '/').equals("a/b"), "getParent");
        check(!rel.isAbsolute(), "relative path");
        check(Path.of("a/b/../c").normalize().toString().replace('\\', '/').equals("a/c"),
                "normalize");
        check(dir.relativize(sub).toString().replace('\\', '/').equals("a/b/c"), "relativize");
        System.out.println("CK RJdkNio files=" + names + " size=" + Files.size(f)
                + " normalize=" + Path.of("a/b/../c").normalize().toString().replace('\\', '/'));
    }

    /**
     * W7-68 §3.2 / W7-72 §2. The synthetic FileChannel private map used to put
     * the fd in AbstractInterruptibleChannel.closeLock (an L slot, where the
     * descriptor coercion degraded the Int to null, so the fd never persisted)
     * and the file position in `closed`, the boolean isOpen() negates. A channel
     * whose position moved off zero reported itself CLOSED. Separately, the
     * winning isOpen() body answered a constant true, so close() never flipped it.
     *
     * BOTH directions are asserted on purpose: a VM answering true everywhere
     * passes the first half, a VM answering false everywhere passes the second.
     * Only one that tracks the transition passes both. position(1) is the
     * sharpest single case -- 1 is what a boolean true reads as.
     *
     * HONEST LIMIT: in Compatible mode a real sun.nio.ch.FileChannelImpl
     * declares all these methods itself and never resolves to CratonVM's
     * natives, so on that path this is a parity green that measures nothing. It
     * fires only when the newFileChannel reconcile-with-real construction fails
     * and the legacy synthetic fallback is taken. It does NOT close W7-68 §3.2;
     * that still needs probes/FileChannelIsOpenProbe.java on a built binary.
     */
    static void fileChannelIsOpenTracksCloseNotPosition(Path dir) throws Exception {
        Path p = dir.resolve("isopen.bin");
        Files.write(p, new byte[] { 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12 });
        FileChannel ch = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE);
        check(ch.isOpen(), "a freshly opened FileChannel must report itself open");

        ch.position(0L);
        check(ch.isOpen(), "isOpen() after position(0)");
        ch.position(1L);
        check(ch.isOpen(), "isOpen() after position(1) -- 1 is what a boolean true reads as");
        check(ch.position() == 1L, "position() must read back 1");
        ch.position(7L);
        check(ch.isOpen(), "isOpen() after position(7)");
        check(ch.position() == 7L, "position() must read back 7");

        ByteBuffer in = ByteBuffer.allocate(2);
        check(ch.read(in) == 2, "read 2 bytes from position 7");
        check(in.array()[0] == 8 && in.array()[1] == 9, "the fd must survive a position move");
        check(ch.isOpen(), "isOpen() after read");

        ch.position(2L);
        check(ch.write(ByteBuffer.wrap(new byte[] { 99 })) == 1, "write 1 byte at position 2");
        check(ch.isOpen(), "isOpen() after write");
        ch.force(true);
        check(ch.isOpen(), "isOpen() after force");
        ch.position(0L);
        check(ch.isOpen(), "isOpen() back at position(0)");

        ch.close();
        check(!ch.isOpen(), "a closed FileChannel must report itself closed");
        ch.close();
        check(!ch.isOpen(), "close() twice must stay closed");

        boolean threw = false;
        try {
            ch.position(3L);
        } catch (ClosedChannelException expected) {
            threw = true;
        }
        check(threw, "position() on a closed channel must throw ClosedChannelException");
        check(Files.readAllBytes(p)[2] == 99, "the write must have reached the file");
        System.out.println("CK RJdkNio fileChannelIsOpen=tracked");
    }

    static void randomAccessAndMapping(Path dir) throws Exception {
        Path f = dir.resolve("raf.bin");
        try (RandomAccessFile raf = new RandomAccessFile(f.toFile(), "rw")) {
            raf.writeInt(0x01020304);
            raf.writeLong(0x1122334455667788L);
            raf.writeUTF("hello");
            raf.writeDouble(1.5d);
            check(raf.length() == 4 + 8 + 7 + 8, "raf length: " + raf.length());
            raf.seek(0);
            check(raf.readInt() == 0x01020304, "readInt");
            check(raf.readLong() == 0x1122334455667788L, "readLong");
            check(raf.readUTF().equals("hello"), "readUTF");
            check(raf.readDouble() == 1.5d, "readDouble");
            check(raf.getFilePointer() == raf.length(), "file pointer at EOF");
            raf.seek(4);
            check(raf.readLong() == 0x1122334455667788L, "seek + read");
            raf.setLength(4);
            check(raf.length() == 4, "setLength truncate");
        }

        // FileChannel: absolute and relative reads/writes, then a memory map.
        Path m = dir.resolve("mapped.bin");
        byte[] payload = new byte[256];
        for (int i = 0; i < payload.length; i++) {
            payload[i] = (byte) i;
        }
        try (FileChannel ch = FileChannel.open(m, StandardOpenOption.CREATE,
                StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            check(ch.write(ByteBuffer.wrap(payload)) == 256, "channel write");
            check(ch.size() == 256, "channel size");
            check(ch.position() == 256, "channel position");
            ch.position(0);
            ByteBuffer in = ByteBuffer.allocate(256);
            check(ch.read(in) == 256, "channel read");
            check(Arrays.equals(in.array(), payload), "channel round-trip");

            // Absolute read does not move the position.
            ByteBuffer abs = ByteBuffer.allocate(4);
            check(ch.read(abs, 16) == 4, "absolute read");
            abs.flip();
            check(abs.get() == 16 && abs.get() == 17, "absolute read content");

            // An exclusive file lock.
            try (FileLock lock = ch.lock()) {
                check(lock.isValid(), "file lock valid");
                check(!lock.isShared(), "exclusive lock");
            }

            // Memory mapping, read/write, then force.
            MappedByteBuffer map = ch.map(FileChannel.MapMode.READ_WRITE, 0, 256);
            check(map.capacity() == 256, "map capacity");
            check(map.isDirect(), "a mapped buffer is direct");
            check(map.get(5) == 5, "mapped read");
            map.put(5, (byte) 99);
            map.force();
            check(map.get(5) == 99, "mapped write visible through the map");
            ByteBuffer verify = ByteBuffer.allocate(1);
            ch.read(verify, 5);
            check(verify.get(0) == 99, "mapped write visible through the channel");

            // Read-only mapping refuses writes.
            MappedByteBuffer ro = ch.map(FileChannel.MapMode.READ_ONLY, 0, 16);
            check(ro.isReadOnly(), "read-only map");
            boolean threw = false;
            try {
                ro.put(0, (byte) 1);
            } catch (java.nio.ReadOnlyBufferException expected) {
                threw = true;
            }
            check(threw, "a READ_ONLY map must refuse writes");

            // transferTo another channel.
            Path t = dir.resolve("transfer.bin");
            try (FileChannel out = FileChannel.open(t, StandardOpenOption.CREATE,
                    StandardOpenOption.WRITE)) {
                check(ch.transferTo(0, 256, out) == 256, "transferTo");
            }
            check(Files.size(t) == 256, "transferred size");
        }
        System.out.println("CK RJdkNio raf+map ok mappedByte=99 transferred=256");
    }

    static void buffers() {
        ByteBuffer b = ByteBuffer.allocate(32);
        check(b.capacity() == 32 && b.position() == 0 && b.limit() == 32, "initial buffer state");
        b.putInt(1).putLong(2L).putShort((short) 3).put((byte) 4);
        check(b.position() == 15, "position after puts: " + b.position());
        b.flip();
        check(b.limit() == 15 && b.position() == 0, "flip");
        check(b.getInt() == 1 && b.getLong() == 2L && b.getShort() == 3 && b.get() == 4,
                "buffer round-trip");
        check(!b.hasRemaining(), "drained");
        b.rewind();
        check(b.position() == 0, "rewind");
        b.mark();
        b.getInt();
        b.reset();
        check(b.position() == 0, "mark/reset");
        b.clear();
        check(b.position() == 0 && b.limit() == 32, "clear");

        // Byte order.
        ByteBuffer be = ByteBuffer.allocate(4).order(java.nio.ByteOrder.BIG_ENDIAN);
        be.putInt(0x01020304).flip();
        check(be.get(0) == 1 && be.get(3) == 4, "big-endian layout");
        ByteBuffer le = ByteBuffer.allocate(4).order(java.nio.ByteOrder.LITTLE_ENDIAN);
        le.putInt(0x01020304).flip();
        check(le.get(0) == 4 && le.get(3) == 1, "little-endian layout");

        // Direct buffers.
        ByteBuffer d = ByteBuffer.allocateDirect(16);
        check(d.isDirect() && !ByteBuffer.allocate(16).isDirect(), "direct flag");
        d.putDouble(2.5d).flip();
        check(d.getDouble() == 2.5d, "direct buffer round-trip");

        // Slices and views alias the parent.
        ByteBuffer parent = ByteBuffer.allocate(16);
        parent.position(4);
        ByteBuffer slice = parent.slice();
        check(slice.capacity() == 12, "slice capacity");
        slice.put(0, (byte) 7);
        check(parent.get(4) == 7, "slice aliases the parent");
        check(parent.asReadOnlyBuffer().isReadOnly(), "asReadOnlyBuffer");

        // Bounds are enforced.
        boolean threw = false;
        try {
            ByteBuffer.allocate(4).getInt(3);
        } catch (IndexOutOfBoundsException expected) {
            threw = true;
        }
        check(threw, "buffer bounds must be enforced");

        // Charset encode/decode round-trip through CharBuffer.
        CharBuffer cb = StandardCharsets.UTF_8.decode(
                ByteBuffer.wrap("héllo".getBytes(StandardCharsets.UTF_8)));
        check(cb.toString().equals("héllo"), "charset decode");
        // Print the decoded string's SHAPE, not the string: stdout's console
        // encoding would otherwise mangle the non-ASCII char differently on
        // different VMs and turn an encoding difference into a false failure.
        System.out.println("CK RJdkNio buffers ok slice=7 decodeLen=" + cb.length()
                + " decodeCp1=" + cb.toString().codePointAt(1));
    }

    static void selectorAndAsyncClose() throws Exception {
        try (ServerSocketChannel server = ServerSocketChannel.open()) {
            server.bind(new InetSocketAddress(java.net.InetAddress.getLoopbackAddress(), 0));
            server.configureBlocking(false);
            try (Selector sel = Selector.open()) {
                check(sel.isOpen(), "selector open");

                // W7-9 §6 / §8.2 -- Selector.provider(). The ninth abstract on
                // java.nio.channels.Selector had no native anywhere in the tree,
                // and CratonVM's Selector.open() hands back an object whose class
                // is the real sun.nio.ch.SelectorImpl allocated WITHOUT running a
                // constructor, so the real (final) AbstractSelector.provider()
                // read an unset field and answered null. The first check is the
                // red: it fails on the old behaviour in BOTH modes. The second is
                // a control (null == null would satisfy it). The third pins the
                // answer to the platform default, which is what makes the
                // openSocketChannel / openDatagramChannel natives native-io
                // registers against the concrete provider classes reachable --
                // returning some other carrier would pass check one and still be
                // wrong. A red on all three means SelectorProvider.provider()
                // itself is unavailable, which is a finding, not a harness bug.
                java.nio.channels.spi.SelectorProvider prov = sel.provider();
                check(prov != null, "Selector.provider() must not be null");
                check(prov == sel.provider(), "Selector.provider() must be stable across calls");
                check(prov == java.nio.channels.spi.SelectorProvider.provider(),
                        "Selector.provider() must be the platform default provider");

                SelectionKey acceptKey = server.register(sel, SelectionKey.OP_ACCEPT);
                check(acceptKey.isValid(), "registration key valid");
                check(sel.keys().size() == 1, "selector key set");
                check(sel.selectNow() == 0, "nothing ready yet");

                try (SocketChannel client = SocketChannel.open()) {
                    client.connect(new InetSocketAddress(
                            java.net.InetAddress.getLoopbackAddress(),
                            ((InetSocketAddress) server.getLocalAddress()).getPort()));
                    int ready = 0;
                    long deadline = System.nanoTime() + T * 1_000_000_000L;
                    while (ready == 0 && System.nanoTime() < deadline) {
                        ready = sel.select(200);
                    }
                    check(ready == 1, "selector must report the pending accept");
                    check(sel.selectedKeys().contains(acceptKey), "selected key");
                    check(acceptKey.isAcceptable(), "OP_ACCEPT ready");
                    sel.selectedKeys().clear();

                    try (SocketChannel accepted = server.accept()) {
                        check(accepted != null, "accept must return a channel");
                        client.write(ByteBuffer.wrap("ping".getBytes(StandardCharsets.UTF_8)));
                        ByteBuffer in = ByteBuffer.allocate(4);
                        accepted.configureBlocking(true);
                        while (in.hasRemaining() && accepted.read(in) >= 0) {
                            // drain
                        }
                        check(new String(in.array(), StandardCharsets.UTF_8).equals("ping"),
                                "loopback channel payload");
                    }
                }

                acceptKey.cancel();
                sel.selectNow();
                check(!acceptKey.isValid(), "cancelled key must be invalid");
            }
        }

        // ASYNCHRONOUS CLOSE: a blocked reader must get AsynchronousCloseException
        // when another thread closes the channel out from under it.
        try (ServerSocketChannel server = ServerSocketChannel.open()) {
            server.bind(new InetSocketAddress(java.net.InetAddress.getLoopbackAddress(), 0));
            int port = ((InetSocketAddress) server.getLocalAddress()).getPort();
            try (SocketChannel client = SocketChannel.open(
                    new InetSocketAddress(java.net.InetAddress.getLoopbackAddress(), port));
                    SocketChannel accepted = server.accept()) {
                CountDownLatch reading = new CountDownLatch(1);
                CountDownLatch done = new CountDownLatch(1);
                AtomicReference<String> outcome = new AtomicReference<>("none");
                Thread reader = new Thread(() -> {
                    reading.countDown();
                    try {
                        client.read(ByteBuffer.allocate(16));
                        outcome.set("returned");
                    } catch (AsynchronousCloseException e) {
                        outcome.set("AsynchronousCloseException");
                    } catch (ClosedChannelException e) {
                        outcome.set("ClosedChannelException");
                    } catch (IOException e) {
                        outcome.set(e.getClass().getSimpleName());
                    }
                    done.countDown();
                });
                reader.setDaemon(true);
                reader.start();
                check(reading.await(T, TimeUnit.SECONDS), "reader never started");
                Thread.sleep(200);   // let the reader block in read()
                client.close();
                check(done.await(T, TimeUnit.SECONDS), "the blocked reader never woke up");
                check(outcome.get().equals("AsynchronousCloseException"),
                        "async close outcome: " + outcome.get());
                check(!client.isOpen(), "channel closed");

                boolean threw = false;
                try {
                    client.read(ByteBuffer.allocate(1));
                } catch (ClosedChannelException expected) {
                    threw = true;
                }
                check(threw, "reading a closed channel must throw ClosedChannelException");
                System.out.println("CK RJdkNio asyncClose=" + outcome.get());
            }
        }
    }

    /**
     * `FileInputStream.skip` is an `lseek`, not a read-and-discard, and the
     * predicate that chooses between them.
     *
     * These four answers were a recorded residual from 2026-08-28 to
     * 2026-09-10, twice diagnosed as a dispatch defect and twice "fixed"
     * inertly, because the real cause was one native reading its RECEIVER as
     * its `FileDescriptor` parameter and therefore answering `isRegularFile0`
     * false for every file ever opened. `skip` is
     * `if (isRegularFile()) skip0(n); else super.skip(n);`, so a false there
     * silently substitutes `InputStream`'s read-and-discard default — which
     * cannot skip past the end and cannot refuse a negative count.
     *
     * The whole point of pinning it here is that neither half is visible from
     * the other: a `skip` that works proves the predicate, and the predicate is
     * private. Ask the observable.
     */
    static void fileInputStreamSkipIsAnLseek(Path dir) throws Exception {
        Path f = dir.resolve("skip.bin");
        Files.write(f, new byte[] { 1, 2 });

        try (FileInputStream in = new FileInputStream(f.toFile())) {
            check(in.read() == 1, "first byte");
            check(in.read() == 2, "second byte");
            check(in.read() == -1, "EOF");
            // PAST the end: lseek succeeds and reports the full count.
            // Read-and-discard answers 0, which is what this vector is here to
            // fail on.
            long skipped = in.skip(4);
            check(skipped == 4, "skip(4) at EOF must report 4, got " + skipped);
            long pos = in.getChannel().position();
            check(pos == 6, "channel position after skipping past EOF must be 6, got " + pos);
        }

        try (FileInputStream in = new FileInputStream(f.toFile())) {
            check(in.skip(2) == 2, "skip(2) over a 2-byte file");
            check(in.read() == -1, "at EOF after skipping the whole file");
            check(in.skip(100) == 100, "skip past the end reports the full count");
        }

        try (FileInputStream in = new FileInputStream(f.toFile())) {
            boolean threw = false;
            try {
                in.skip(-1);
            } catch (IOException expected) {
                threw = true;
            }
            // lseek before the start fails EINVAL. Answering 0 makes a rewind
            // attempt look like a no-op that succeeded.
            check(threw, "skip(-1) at position 0 must throw IOException");
        }

        Files.delete(f);
        System.out.println("CK RJdkNio skipIsAnLseek");
    }

    /**
     * A `Path` this VM hands out carries the platform implementation's own
     * instance layout, not a private two-slot one.
     *
     * `getClass()` reports `sun.nio.fs.UnixPath` / `WindowsPath`, and until
     * 2026-09-10 the natives wrote the path STRING into that class's `fs` slot
     * and the owning filesystem into its `byte[] path` slot — invisible for
     * exactly as long as this VM's own natives were the only readers, and 102
     * differences against HotSpot the moment real bytecode read one.
     *
     * Asserted from Java, with no reflection: every method below is one the
     * real class implements out of a field the old layout left null, so a
     * regression shows up as a wrong ANSWER rather than as an internals check
     * that a future JDK could invalidate.
     */
    static void pathCarriesTheRealLayout(Path dir) throws Exception {
        Path p = Path.of("a/b");
        // Separator-normalised, the same way `normalize=` is done at line 166
        // of this file. On Windows BOTH VMs answer `a\b`, byte-identical, so
        // asserting the POSIX spelling fails the HOTSPOT ORACLE -- and an oracle
        // that cannot pass voids the whole cross-VM diff for this vector, which
        // is how it read as a VM regression for two binaries running. Verified on
        // both VMs before relaxing it: the path STRUCTURE is still asserted, only
        // the host's separator is not.
        check(p.toString().replace('\\', '/').equals("a/b"), "Path.toString: " + p);
        check(p.equals(Path.of("a/b")), "two equal Paths must be equal");
        check(p.hashCode() == Path.of("a/b").hashCode(), "equal Paths share a hashCode");
        check(p.getFileName().toString().equals("b"), "getFileName: " + p.getFileName());
        check(p.getParent().toString().equals("a"), "getParent: " + p.getParent());
        check(p.getNameCount() == 2, "getNameCount: " + p.getNameCount());
        check(p.compareTo(Path.of("a/b")) == 0, "compareTo self");
        check(p.resolve("c").toString().replace('\\', '/').equals("a/b/c"),
                "resolve: " + p.resolve("c"));
        // A Path built by the walk/attribute machinery rather than by `of`,
        // because those are separate producers of the same carrier and a slot
        // map they do not share is how the two drift.
        Path made = dir.resolve("layout.txt");
        Files.write(made, new byte[] { 7 });
        check(made.getFileName().toString().equals("layout.txt"), "resolved Path filename");
        check(Files.size(made) == 1, "a Path from resolve() still reaches the file");
        try (DirectoryStream<Path> ds = Files.newDirectoryStream(dir)) {
            boolean seen = false;
            for (Path e : ds) {
                if (e.getFileName().toString().equals("layout.txt")) {
                    seen = true;
                    check(Files.size(e) == 1, "a Path from a DirectoryStream reaches the file");
                }
            }
            check(seen, "the directory stream must yield the file just written");
        }
        Files.delete(made);
        System.out.println("CK RJdkNio pathLayout");
    }

    /**
     * The default `FileSystemProvider` is a class `new` could have produced.
     *
     * It was minted as the ABSTRACT `java.nio.file.spi.FileSystemProvider`
     * itself (JVMS 6.5 makes that an `InstantiationError`), which is why
     * `Files.probeContentType` died with a `NoSuchMethodError` naming
     * `getFileTypeDetector()`: the real bytecode calls it on a
     * `sun.nio.fs.UnixFileSystemProvider` and the abstract base declares no
     * such method.
     */
    static void defaultProviderIsConcrete(Path dir) throws Exception {
        java.nio.file.spi.FileSystemProvider prov = FileSystems.getDefault().provider();
        check(prov != null, "the default filesystem has a provider");
        check(!java.lang.reflect.Modifier.isAbstract(prov.getClass().getModifiers()),
                "the default provider's class must not be abstract: " + prov.getClass().getName());
        check(!prov.getClass().isInterface(),
                "the default provider's class must not be an interface: " + prov.getClass().getName());
        check("file".equals(prov.getScheme()), "default provider scheme: " + prov.getScheme());
        // The call that died. Its ANSWER is host-dependent (null is legal when
        // the host has no mime.types), so what is asserted is that it returns
        // at all rather than what it returns.
        Path t = dir.resolve("probe.txt");
        Files.write(t, "hello".getBytes(StandardCharsets.UTF_8));
        Files.probeContentType(t);
        Files.delete(t);
        System.out.println("CK RJdkNio providerIsConcrete");
    }

    public static void main(String[] args) throws Exception {
        Path dir = java.nio.file.Files.createTempDirectory("rjdknio");
        try {
            filesApi(dir);
            fileInputStreamSkipIsAnLseek(dir);
            pathCarriesTheRealLayout(dir);
            defaultProviderIsConcrete(dir);
            randomAccessAndMapping(dir);
            fileChannelIsOpenTracksCloseNotPosition(dir);
            buffers();
            selectorAndAsyncClose();
        } finally {
            deleteTree(dir);
        }
        System.out.println("CK RJdkNio checks=" + checks);
        System.out.println("PASS RJdkNio (" + checks + " checks)");
    }
}
