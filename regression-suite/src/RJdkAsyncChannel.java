import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.channels.AsynchronousFileChannel;
import java.nio.channels.CompletionHandler;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

/**
 * JDK-only corpus: {@link AsynchronousFileChannel} and the {@code Future} its
 * positional read/write hand back.
 *
 * WHY THIS VECTOR EXISTS. CratonVM's `AsynchronousFileChannel.read(ByteBuffer,
 * long)` / `write(ByteBuffer, long)` natives are registered `Bridge`, so they
 * SURVIVE `--jdk-only`, and they used to wrap their result in a VM-minted class
 * named `java.util.concurrent.CompletedFuture` -- a name NO JDK image declares
 * (it is listed in native-api's `NO_IMAGE_JDK_RECEIVERS`; note that the JDK's
 * own private completed-future class is `sun.nio.ch.CompletedFuture`, in a
 * different package). Strict mode correctly refuses to mint it, so the refusal
 * came out at the APPLICATION's call site as
 * `NoClassDefFoundError: java/util/concurrent/CompletedFuture` on
 * `ch.write(buf, 0).get()`, where HotSpot 25 runs the identical program
 * cleanly. H2's `FileAsync.write` / `TestFileSystem.testConcurrent` on the
 * `async:` filesystem is the corpus vector for the same code path.
 *
 * NON-NULL IS NOT THE CONTRACT. `check(future != null)` and a check count both
 * pass on the broken VM, so the assertions here are about the returned object's
 * IDENTITY (a real, JDK-declared class with declared methods, not a two-slot
 * stand-in wearing a `java.util.concurrent` name) and about the BYTES actually
 * reaching the file, read back after close through an unrelated API.
 *
 * DETERMINISM. Everything happens under a fresh temp directory whose absolute
 * path is never printed, and no CK line carries a class name: the returned
 * future's class legitimately differs between VMs (HotSpot answers
 * `sun.nio.ch.PendingFuture`), so only the VERDICT is printed, never the name.
 */
public class RJdkAsyncChannel {
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
                    // Cleanup is best-effort and never part of the assertions.
                }
            }
        } catch (IOException ignored) {
            // Cleanup is best-effort and never part of the assertions.
        }
    }

    /**
     * The returned object must be a real JDK class, not a VM-minted stand-in.
     *
     * Four independent questions, because each alone has a way to pass on a
     * fabricated object: the package prefix (a stand-in can still be named out
     * of `java.util.concurrent`), the exact fabricated name, whether the class
     * DECLARES anything (a VM stand-in is a bare N-slot shape whose whole
     * behaviour is registered natives, so it declares no methods of its own),
     * and whether it actually implements `Future`.
     *
     * The class NAME is deliberately not printed on the happy path -- it
     * differs legitimately between VMs and the harness diffs CK lines.
     */
    static void checkRealFutureClass(Future<?> f, String where) {
        Class<?> cls = f.getClass();
        String cn = cls.getName();
        check(!cn.startsWith("cratonvm"), where + ": future class is VM-internal: " + cn);
        check(cn.startsWith("java.") || cn.startsWith("jdk.") || cn.startsWith("sun."),
                where + ": future class is not a JDK name: " + cn);
        check(!cn.equals("java.util.concurrent.CompletedFuture"),
                where + ": java.util.concurrent.CompletedFuture is a class no JDK image "
                        + "declares -- the strict-mode refusal of it is correct, the caller "
                        + "that asks for it is the defect");
        check(cls.getDeclaredMethods().length > 0,
                where + ": " + cn + " declares no methods of its own, which is the shape of a "
                        + "fabricated stand-in rather than a JDK class");
        check(Future.class.isAssignableFrom(cls), where + ": " + cn + " does not implement Future");
    }

    /**
     * The whole `Future` contract for an operation that has already finished.
     * A future that is done and not cancelled must refuse cancellation, must
     * keep answering with the same value afterwards, and must satisfy the timed
     * `get` without waiting.
     */
    static void checkCompletedFuture(Future<Integer> f, int expected, String where)
            throws Exception {
        checkRealFutureClass(f, where);
        check(f.get() == expected, where + ": get() = " + f.get() + ", expected " + expected);
        check(f.isDone(), where + ": a returned future must already be done");
        check(!f.isCancelled(), where + ": a completed future must not report itself cancelled");
        check(f.get(T, TimeUnit.SECONDS) == expected, where + ": timed get()");
        check(!f.cancel(true), where + ": cancel() on an already-completed future must return false");
        check(!f.isCancelled(), where + ": a refused cancel must not flip isCancelled()");
        check(f.isDone(), where + ": a refused cancel must not clear isDone()");
        check(f.get() == expected, where + ": the value must survive a refused cancel");
    }

    /** Positional write/read, the Future contract, and the bytes on disk. */
    static void writeReadRoundTrip(Path dir) throws Exception {
        Path f = dir.resolve("roundtrip.bin");
        Files.createFile(f);
        byte[] first = new byte[] { 1, 2, 3, 4 };
        byte[] second = new byte[] { 5, 6, 7, 8 };

        try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(f,
                StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            check(ch.isOpen(), "a freshly opened AsynchronousFileChannel must report itself open");

            Future<Integer> w0 = ch.write(ByteBuffer.wrap(first), 0);
            checkCompletedFuture(w0, 4, "write@0");
            Future<Integer> w4 = ch.write(ByteBuffer.wrap(second), 4);
            checkCompletedFuture(w4, 4, "write@4");
            check(ch.size() == 8, "size after two positional writes: " + ch.size());

            ch.force(true);
            ch.force(false);
            check(ch.isOpen(), "force() must not close the channel");

            ByteBuffer all = ByteBuffer.allocate(8);
            Future<Integer> r0 = ch.read(all, 0);
            checkCompletedFuture(r0, 8, "read@0");
            check(all.position() == 8, "a completed read must advance the buffer: " + all.position());
            check(Arrays.equals(all.array(), new byte[] { 1, 2, 3, 4, 5, 6, 7, 8 }),
                    "read@0 contents: " + Arrays.toString(all.array()));

            // A positional read is absolute: it must see exactly the second
            // write's bytes and must not have been disturbed by the first.
            ByteBuffer tail = ByteBuffer.allocate(4);
            Future<Integer> r4 = ch.read(tail, 4);
            checkCompletedFuture(r4, 4, "read@4");
            check(Arrays.equals(tail.array(), second),
                    "read@4 contents: " + Arrays.toString(tail.array()));

            // Reading at EOF is -1, not 0 and not an exception.
            ByteBuffer eof = ByteBuffer.allocate(4);
            Future<Integer> rEof = ch.read(eof, 8);
            checkCompletedFuture(rEof, -1, "read@EOF");

            // A negative position is an IllegalArgumentException from the call
            // itself, not a failed future.
            boolean threw = false;
            try {
                ch.read(ByteBuffer.allocate(4), -1L);
            } catch (IllegalArgumentException expected) {
                threw = true;
            }
            check(threw, "a negative read position must throw IllegalArgumentException");
            threw = false;
            try {
                ch.write(ByteBuffer.wrap(first), -1L);
            } catch (IllegalArgumentException expected) {
                threw = true;
            }
            check(threw, "a negative write position must throw IllegalArgumentException");
        }

        // The bytes must be on disk, read back through an unrelated API after
        // the channel is gone. This is the check a future that merely REPORTS 4
        // cannot pass.
        byte[] onDisk = Files.readAllBytes(f);
        check(Arrays.equals(onDisk, new byte[] { 1, 2, 3, 4, 5, 6, 7, 8 }),
                "file contents after close: " + Arrays.toString(onDisk));
        System.out.println("CK RJdkAsyncChannel roundTrip=" + Arrays.toString(onDisk));
    }

    /** size(), truncate(), close() and the closed-channel state. */
    static void sizeTruncateClose(Path dir) throws Exception {
        Path f = dir.resolve("truncate.bin");
        Files.createFile(f);
        AsynchronousFileChannel ch = AsynchronousFileChannel.open(f,
                StandardOpenOption.READ, StandardOpenOption.WRITE);
        try {
            check(ch.size() == 0, "a fresh empty file has size 0: " + ch.size());
            Future<Integer> w = ch.write(ByteBuffer.wrap(new byte[] { 9, 8, 7, 6 }), 0);
            checkCompletedFuture(w, 4, "truncate-file write");
            check(ch.size() == 4, "size after write: " + ch.size());

            // truncate returns THIS channel, per the AsynchronousFileChannel
            // contract -- a fresh object here would break `open(..).truncate(..)`
            // chaining.
            AsynchronousFileChannel t = ch.truncate(2L);
            check(t == ch, "truncate() must return the same channel");
            check(ch.size() == 2, "size after truncate(2): " + ch.size());

            // Truncating to a length at or above the current size is a no-op.
            ch.truncate(9L);
            check(ch.size() == 2, "truncate() above the current size must not grow: " + ch.size());

            boolean threw = false;
            try {
                ch.truncate(-1L);
            } catch (IllegalArgumentException expected) {
                threw = true;
            }
            check(threw, "a negative truncate size must throw IllegalArgumentException");
        } finally {
            ch.close();
        }
        check(!ch.isOpen(), "a closed AsynchronousFileChannel must report itself closed");
        ch.close();
        check(!ch.isOpen(), "close() twice must stay closed");

        byte[] onDisk = Files.readAllBytes(f);
        check(Arrays.equals(onDisk, new byte[] { 9, 8 }),
                "file contents after truncate: " + Arrays.toString(onDisk));
        System.out.println("CK RJdkAsyncChannel truncated=" + Arrays.toString(onDisk));
    }

    /**
     * The CompletionHandler overloads. They deliver the byte count as a boxed
     * `Integer` and hand back the caller's attachment unchanged; the handler
     * may run on another thread, so both arms are latched.
     */
    static void completionHandlers(Path dir) throws Exception {
        Path f = dir.resolve("handler.bin");
        Files.createFile(f);
        byte[] payload = new byte[] { 10, 20, 30, 40 };
        final Object attachment = new Object();

        try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(f,
                StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            CountDownLatch wrote = new CountDownLatch(1);
            AtomicReference<Integer> wroteN = new AtomicReference<>(null);
            AtomicReference<Object> wroteAtt = new AtomicReference<>(null);
            AtomicReference<Throwable> wroteErr = new AtomicReference<>(null);
            ch.write(ByteBuffer.wrap(payload), 0, attachment,
                    new CompletionHandler<Integer, Object>() {
                        @Override
                        public void completed(Integer result, Object att) {
                            wroteN.set(result);
                            wroteAtt.set(att);
                            wrote.countDown();
                        }

                        @Override
                        public void failed(Throwable exc, Object att) {
                            wroteErr.set(exc);
                            wrote.countDown();
                        }
                    });
            check(wrote.await(T, TimeUnit.SECONDS), "the write CompletionHandler never fired");
            check(wroteErr.get() == null, "write handler failed: " + wroteErr.get());
            check(wroteN.get() != null && wroteN.get() == 4,
                    "write handler byte count: " + wroteN.get());
            check(wroteAtt.get() == attachment, "the write attachment must be handed back unchanged");

            CountDownLatch read = new CountDownLatch(1);
            ByteBuffer dst = ByteBuffer.allocate(4);
            AtomicReference<Integer> readN = new AtomicReference<>(null);
            AtomicReference<Object> readAtt = new AtomicReference<>(null);
            AtomicReference<Throwable> readErr = new AtomicReference<>(null);
            ch.read(dst, 0, attachment, new CompletionHandler<Integer, Object>() {
                @Override
                public void completed(Integer result, Object att) {
                    readN.set(result);
                    readAtt.set(att);
                    read.countDown();
                }

                @Override
                public void failed(Throwable exc, Object att) {
                    readErr.set(exc);
                    read.countDown();
                }
            });
            check(read.await(T, TimeUnit.SECONDS), "the read CompletionHandler never fired");
            check(readErr.get() == null, "read handler failed: " + readErr.get());
            check(readN.get() != null && readN.get() == 4,
                    "read handler byte count: " + readN.get());
            check(readAtt.get() == attachment, "the read attachment must be handed back unchanged");
            check(Arrays.equals(dst.array(), payload),
                    "read handler contents: " + Arrays.toString(dst.array()));
        }

        byte[] onDisk = Files.readAllBytes(f);
        check(Arrays.equals(onDisk, payload),
                "handler-written file contents: " + Arrays.toString(onDisk));
        System.out.println("CK RJdkAsyncChannel handler=" + Arrays.toString(onDisk));
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("rjdkasyncchannel");
        try {
            writeReadRoundTrip(dir);
            sizeTruncateClose(dir);
            completionHandlers(dir);
        } finally {
            deleteTree(dir);
        }
        System.out.println("CK RJdkAsyncChannel checks=" + checks);
        System.out.println("PASS RJdkAsyncChannel (" + checks + " checks)");
    }
}
