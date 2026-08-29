import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.channels.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicReference;

/** L6's fourth assignment: `AsynchronousFileChannel`, recorded OPEN in
 *  `the-roadmaps-phase-1-and-3-re-adjudicated-and-six-fixes-20260827` §6 as
 *
 *      `AsynchronousFileChannel.write` returns a `CompletableFuture` where
 *      HotSpot returns `sun.nio.ch.PendingFuture`. Every value agrees; the
 *      type does not.
 *
 *  The lane doc asks the question the record did not: the type gap "implies
 *  the async channel completes synchronously, which is worth confirming before
 *  deciding whether the type matters." So this probe asks the CONSEQUENCES of
 *  a synchronously-completed future rather than its class name — whether the
 *  Future contract holds, whether the CompletionHandler form is reached, and
 *  whether the two forms agree with each other and with the file on disk.
 *
 *  DETERMINISM: no path, no size, no timing and no thread name is printed. The
 *  temp file is created and deleted by the probe, and every assertion is over
 *  bytes it wrote itself. `Future.isDone()` IS printed, and deliberately: for a
 *  channel that completes synchronously it is `true` immediately and for one
 *  that does not it may be either — so the row is read as a statement about
 *  THIS VM, and the diff against HotSpot is the whole point.
 */
public class AsyncChannelSweep {
    static int rows = 0;
    static void p(String tag, Object v) {
        rows++;
        System.out.println(tag + " |" + String.valueOf(v) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Throwable; }

    public static void main(String[] args) throws Exception {
        Path f = Files.createTempFile("l6afc", ".bin");
        try {
            try (AsynchronousFileChannel ch = AsynchronousFileChannel.open(
                    f, StandardOpenOption.WRITE, StandardOpenOption.READ)) {
                p("channel isOpen", ch.isOpen());

                // ---- the Future form -------------------------------------
                ByteBuffer w = ByteBuffer.wrap(new byte[] { 1, 2, 3, 4 });
                Future<Integer> fw = ch.write(w, 0);
                p("write future non-null", fw != null);
                p("write future isCancelled", fw.isCancelled());
                p("write future get", fw.get());
                p("write future isDone after get", fw.isDone());
                p("write future get twice agrees", fw.get());
                p("write future cancel after done", fw.cancel(true));
                p("write future timed get", fw.get(5, TimeUnit.SECONDS));
                p("buffer fully consumed", w.remaining());

                p("size after write", ch.size());
                t("force(true)", () -> ch.force(true));

                ByteBuffer r = ByteBuffer.allocate(4);
                Future<Integer> fr = ch.read(r, 0);
                p("read future get", fr.get());
                p("read future isDone", fr.isDone());
                p("read contents", Arrays.toString(r.array()));

                // Reading past the end is -1, not an exception.
                ByteBuffer eof = ByteBuffer.allocate(4);
                p("read at EOF", ch.read(eof, 64).get());

                // ---- the CompletionHandler form --------------------------
                final AtomicReference<String> completed = new AtomicReference<>("not-called");
                final CountDownLatch done = new CountDownLatch(1);
                ch.write(ByteBuffer.wrap(new byte[] { 9, 9 }), 4, "att",
                    new CompletionHandler<Integer, String>() {
                        public void completed(Integer n, String a) {
                            completed.set("completed n=" + n + " att=" + a);
                            done.countDown();
                        }
                        public void failed(Throwable e, String a) {
                            completed.set("failed " + e.getClass().getName());
                            done.countDown();
                        }
                    });
                p("handler ran within 5s", done.await(5, TimeUnit.SECONDS));
                p("handler saw", completed.get());
                p("size after handler write", ch.size());

                final AtomicReference<String> readSaw = new AtomicReference<>("not-called");
                final CountDownLatch rdone = new CountDownLatch(1);
                final ByteBuffer rb = ByteBuffer.allocate(6);
                ch.read(rb, 0, null, new CompletionHandler<Integer, Object>() {
                    public void completed(Integer n, Object a) {
                        readSaw.set("n=" + n + " " + Arrays.toString(rb.array()) + " att=" + a);
                        rdone.countDown();
                    }
                    public void failed(Throwable e, Object a) {
                        readSaw.set("failed " + e.getClass().getName());
                        rdone.countDown();
                    }
                });
                p("read handler ran within 5s", rdone.await(5, TimeUnit.SECONDS));
                p("read handler saw", readSaw.get());

                // ---- the argument contract -------------------------------
                t("write null buffer", () -> ch.write(null, 0));
                t("write negative position", () -> ch.write(ByteBuffer.allocate(1), -1));
                t("read null buffer", () -> ch.read(null, 0));
                t("read negative position", () -> ch.read(ByteBuffer.allocate(1), -1));
                t("read into a read-only buffer",
                  () -> ch.read(ByteBuffer.allocate(1).asReadOnlyBuffer(), 0));
                t("write with a null handler",
                  () -> ch.write(ByteBuffer.allocate(1), 0, "a", null));
                t("truncate(-1)", () -> ch.truncate(-1));
                p("truncate to 4 then size", ch.truncate(4).size());
                t("lock then release", () -> ch.lock().get().release());
                t("tryLock then release", () -> {
                    FileLock l = ch.tryLock();
                    if (l != null) l.release();
                });
            }

            // ---- after close ------------------------------------------------
            AsynchronousFileChannel closed = AsynchronousFileChannel.open(f, StandardOpenOption.READ);
            closed.close();
            p("isOpen after close", closed.isOpen());
            t("read after close", () -> closed.read(ByteBuffer.allocate(1), 0));
            t("size after close", () -> closed.size());
            t("close twice", () -> closed.close());

            // ---- open-option refusals ---------------------------------------
            t("open APPEND", () -> AsynchronousFileChannel.open(f, StandardOpenOption.APPEND).close());
            t("open a missing file for reading",
              () -> AsynchronousFileChannel.open(f.resolveSibling("l6-absent-xyz"),
                                                 StandardOpenOption.READ).close());
            t("write to a READ-only channel", () -> {
                try (AsynchronousFileChannel ro =
                         AsynchronousFileChannel.open(f, StandardOpenOption.READ)) {
                    ro.write(ByteBuffer.allocate(1), 0).get();
                }
            });
            p("file contents at the end", Arrays.toString(Files.readAllBytes(f)));
        } finally {
            Files.deleteIfExists(f);
        }
        System.out.println("rows " + rows + " DONE AsyncChannelSweep");
    }
}
