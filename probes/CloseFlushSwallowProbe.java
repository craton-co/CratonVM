import java.io.BufferedOutputStream;
import java.io.BufferedWriter;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.io.PrintWriter;
import java.io.PushbackInputStream;
import java.io.Writer;
import java.util.ArrayList;
import java.util.List;
import java.util.Properties;
import java.util.logging.LogRecord;
import java.util.logging.Level;
import java.util.logging.SimpleFormatter;
import java.util.logging.StreamHandler;
import java.util.zip.GZIPOutputStream;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

/**
 * W7-57 — a delegated `close()` / `flush()` must deliver its failure.
 *
 * <p>The defect this probes for is a native that spells its delegation
 * {@code let _ = ctx.invoke_virtual(target, "close", "()V", &[])} and drops the
 * whole {@code Result}. That is not a lost exception: on a {@code close()}
 * after buffered writes it is <b>lost data reported as success</b>, because the
 * caller's {@code try}-with-resources sees a clean exit.
 *
 * <p>So every assertion here is that the failure <b>arrives</b>. A check that
 * passed because nothing was thrown would be the defect's own shape.
 *
 * <p>The other half is just as load-bearing. Some swallows are CORRECT:
 * {@code PrintWriter.close()} really is
 * {@code try { out.close(); } catch (IOException x) { trouble = true; }}, and
 * {@code StreamHandler.close()} really does absorb {@code Exception} into its
 * {@code ErrorManager}. Converting those into throws would be a new defect and
 * a far more visible one. Each absorbing check below is therefore paired with
 * an {@code Error} check on the same call site: the JDK's {@code catch} names
 * {@code IOException} (or {@code Exception}) and neither catches an
 * {@code Error}, so a {@code NoSuchMethodError} — which in this VM means our
 * own dispatch failed to find the method — must still come out.
 *
 * <p>Every expected value below was measured on HotSpot 25.0.3.9 (Eclipse
 * Adoptium) before it was written down; it prints {@code RESULT ok} there
 * today — 23 printed lines, 22 of them asserted. {@code observed.*} is printed
 * and NOT asserted, because it names a divergence this lane did not close
 * (HotSpot sets {@code trouble} for {@code checkError()}; we do not).
 *
 * <p><b>Run it in BOTH modes.</b> Several of the natives these checks land on
 * are registered only under {@code --synthetic-jdk}; in Compatible mode the
 * real bytecode runs and the same check is green for a reason that has nothing
 * to do with the repair. A green Compatible-mode run is therefore not evidence
 * that a synthetic-mode body was fixed, and vice versa.
 *
 * <p>Run: {@code java probes/CloseFlushSwallowProbe.java}
 */
public class CloseFlushSwallowProbe {

    // ---------------------------------------------------------------- sinks

    /** An OutputStream that fails on a chosen operation with a chosen throwable. */
    static final class BoomOut extends OutputStream {
        enum Where { NONE, CLOSE, FLUSH }

        private final Where where;
        private final Throwable what;
        final ByteArrayOutputStream sink = new ByteArrayOutputStream();
        boolean closeAttempted;
        boolean flushAttempted;

        BoomOut(Where where, Throwable what) {
            this.where = where;
            this.what = what;
        }

        @Override public void write(int b) { sink.write(b); }
        @Override public void write(byte[] b, int off, int len) { sink.write(b, off, len); }

        @Override public void flush() throws IOException {
            flushAttempted = true;
            if (where == Where.FLUSH) { raise(); }
        }

        @Override public void close() throws IOException {
            closeAttempted = true;
            if (where == Where.CLOSE) { raise(); }
        }

        private void raise() throws IOException {
            if (what instanceof IOException) { throw (IOException) what; }
            if (what instanceof RuntimeException) { throw (RuntimeException) what; }
            if (what instanceof Error) { throw (Error) what; }
            throw new IllegalStateException("unreachable");
        }
    }

    /** An InputStream whose close() fails. */
    static final class BoomIn extends InputStream {
        private final Throwable what;
        boolean closeAttempted;

        BoomIn(Throwable what) { this.what = what; }

        @Override public int read() { return -1; }

        @Override public void close() throws IOException {
            closeAttempted = true;
            if (what instanceof IOException) { throw (IOException) what; }
            if (what instanceof Error) { throw (Error) what; }
        }
    }

    /** A Writer whose close()/flush() fails. */
    static final class BoomWriter extends Writer {
        private final Throwable what;
        boolean closeAttempted;

        BoomWriter(Throwable what) { this.what = what; }

        @Override public void write(char[] cbuf, int off, int len) { }
        @Override public void flush() { }

        @Override public void close() throws IOException {
            closeAttempted = true;
            if (what instanceof IOException) { throw (IOException) what; }
            if (what instanceof Error) { throw (Error) what; }
        }
    }

    // ------------------------------------------------------------- harness

    private static final List<String> FAILURES = new ArrayList<>();

    /**
     * Renders what a call did: the throwable's `type: message`, or "none".
     *
     * <p>A checked `IOException` cannot escape a `Runnable`, so the cases below
     * re-throw it inside {@link Wrapped}; this unwraps it so the assertion is
     * against the real type. Nothing else is unwrapped — an `Error` and a
     * `RuntimeException` travel out of a lambda unchanged, and the whole point
     * of this probe is which one arrives.
     */
    static String outcome(Runnable body) {
        try {
            body.run();
            return "none";
        } catch (Wrapped w) {
            Throwable real = w.real;
            return real.getClass().getName() + ": " + real.getMessage();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static void check(String name, String expected, String actual) {
        System.out.println(name + "=" + actual);
        if (!expected.equals(actual)) {
            FAILURES.add(name + " expected <" + expected + "> got <" + actual + ">");
        }
    }

    static void check(String name, boolean expected, boolean actual) {
        check(name, String.valueOf(expected), String.valueOf(actual));
    }

    // --------------------------------------------------------------- cases

    /**
     * `FilterOutputStream.close()` catches nothing it does not rethrow: it
     * records a flush failure, rethrows it, and closes `out` in the `finally`.
     * A close failure therefore reaches the caller.
     */
    static void filterOutputStreamClose() {
        BoomOut errBoom = new BoomOut(BoomOut.Where.CLOSE, new Error("close-boom"));
        check("bufferedOutClosePropagatesError",
                "java.lang.Error: close-boom",
                outcome(() -> {
                    try (BufferedOutputStream b = new BufferedOutputStream(errBoom)) {
                        b.write('x');
                    } catch (IOException e) {
                        throw new Wrapped(e);
                    }
                }));
        check("bufferedOutCloseWasAttempted", true, errBoom.closeAttempted);

        BoomOut ioBoom = new BoomOut(BoomOut.Where.CLOSE, new IOException("close-io"));
        check("bufferedOutClosePropagatesIOException",
                "java.io.IOException: close-io",
                outcome(() -> {
                    try (BufferedOutputStream b = new BufferedOutputStream(ioBoom)) {
                        b.write('x');
                    } catch (IOException e) {
                        throw new Wrapped(e);
                    }
                }));

        // The flush failure WINS: FilterOutputStream rethrows it from the
        // `catch (Throwable)` and only suppresses a close failure into it.
        BoomOut bothBoom = new BoomOut(BoomOut.Where.FLUSH, new Error("flush-boom"));
        check("filterOutFlushFailureWins",
                "java.lang.Error: flush-boom",
                outcome(() -> {
                    BufferedOutputStream b = new BufferedOutputStream(bothBoom);
                    try {
                        b.write('x');
                    } catch (IOException e) {
                        throw new Wrapped(e);
                    }
                    try {
                        b.close();
                    } catch (IOException e) {
                        throw new Wrapped(e);
                    }
                }));
        // …and the close is still attempted, from the `finally`.
        check("filterOutCloseAttemptedAfterFailedFlush", true, bothBoom.closeAttempted);
    }

    /**
     * `DataOutputStream.close()` inherits `FilterOutputStream.close()`, so the
     * flush it performs first is a propagating one.
     */
    static void dataOutputStreamClose() {
        BoomOut boom = new BoomOut(BoomOut.Where.FLUSH, new Error("dos-flush-boom"));
        check("dataOutClosePropagatesFlushError",
                "java.lang.Error: dos-flush-boom",
                outcome(() -> {
                    DataOutputStream d = new DataOutputStream(boom);
                    try {
                        d.writeInt(7);
                        d.close();
                    } catch (IOException e) {
                        throw new Wrapped(e);
                    }
                }));
    }

    /**
     * `InputStreamReader.close()` is a bare `sd.close()` over a
     * `StreamDecoder.implClose()` that is a bare `in.close()`.
     */
    static void inputStreamReaderClose() {
        BoomIn boom = new BoomIn(new Error("isr-close-boom"));
        check("inputStreamReaderClosePropagatesError",
                "java.lang.Error: isr-close-boom",
                outcome(() -> {
                    try {
                        new InputStreamReader(boom, "UTF-8").close();
                    } catch (IOException e) {
                        throw new Wrapped(e);
                    }
                }));
        check("inputStreamReaderCloseWasAttempted", true, boom.closeAttempted);
    }

    /** `PushbackInputStream.close()` is `in.close()`, then the fields are nulled. */
    static void pushbackClose() {
        BoomIn boom = new BoomIn(new Error("pushback-close-boom"));
        check("pushbackClosePropagatesError",
                "java.lang.Error: pushback-close-boom",
                outcome(() -> {
                    try {
                        new PushbackInputStream(boom).close();
                    } catch (IOException e) {
                        throw new Wrapped(e);
                    }
                }));
    }

    /**
     * `DeflaterOutputStream.close()` runs `out.close()` in its `finally` under
     * `throws IOException`, so a zip/gzip archive that never reached the sink
     * says so.
     */
    static void zipAndGzipClose() {
        BoomOut gzBoom = new BoomOut(BoomOut.Where.CLOSE, new Error("gzip-close-boom"));
        check("gzipClosePropagatesError",
                "java.lang.Error: gzip-close-boom",
                outcome(() -> {
                    try {
                        GZIPOutputStream g = new GZIPOutputStream(gzBoom);
                        g.write("payload".getBytes("UTF-8"));
                        g.close();
                    } catch (IOException e) {
                        throw new Wrapped(e);
                    }
                }));

        BoomOut zipBoom = new BoomOut(BoomOut.Where.CLOSE, new Error("zip-close-boom"));
        check("zipOutClosePropagatesError",
                "java.lang.Error: zip-close-boom",
                outcome(() -> {
                    try {
                        ZipOutputStream z = new ZipOutputStream(zipBoom);
                        z.putNextEntry(new ZipEntry("a.txt"));
                        z.write("payload".getBytes("UTF-8"));
                        z.closeEntry();
                        z.close();
                    } catch (IOException e) {
                        throw new Wrapped(e);
                    }
                }));
    }

    /**
     * `Properties.store0` ends in a bare `bw.flush()`. On the `BufferedWriter`
     * it wraps the sink in, that flush IS the byte delivery — a dropped failure
     * here reports success over a file that was never written.
     */
    static void propertiesStoreFlush() {
        BoomOut boom = new BoomOut(BoomOut.Where.FLUSH, new IOException("store-flush-io"));
        Properties p = new Properties();
        p.setProperty("k", "v");
        check("propertiesStoreStreamPropagatesFlushIOException",
                "java.io.IOException: store-flush-io",
                outcome(() -> {
                    try {
                        p.store(boom, "hdr");
                    } catch (IOException e) {
                        throw new Wrapped(e);
                    }
                }));

        Writer wboom = new Writer() {
            @Override public void write(char[] cbuf, int off, int len) { }
            @Override public void flush() throws IOException {
                throw new IOException("store-writer-flush-io");
            }
            @Override public void close() { }
        };
        check("propertiesStoreWriterPropagatesFlushIOException",
                "java.io.IOException: store-writer-flush-io",
                outcome(() -> {
                    try {
                        p.store(new BufferedWriter(wboom), "hdr");
                    } catch (IOException e) {
                        throw new Wrapped(e);
                    }
                }));
    }

    /**
     * The counter-check against over-correction, and the narrowing beside it.
     *
     * <p>`PrintWriter.close()` is
     * `try { out.close(); out = null; } catch (IOException x) { trouble = true; }`
     * — the absorb is the JDK's, and `PrintWriter` declares no checked
     * exception, so making this propagate would be a fresh divergence. But that
     * `catch` names `IOException` and nothing wider, so an `Error` still comes
     * out. Both halves are asserted; a fix that satisfies one and not the other
     * has gone wrong in one of the two available directions.
     */
    static void printWriterCloseIsNarrow() {
        BoomWriter ioBoom = new BoomWriter(new IOException("pw-close-io"));
        PrintWriter pwIo = new PrintWriter(ioBoom);
        check("printWriterCloseAbsorbsIOException", "none", outcome(pwIo::close));
        check("printWriterCloseWasAttemptedDespiteAbsorb", true, ioBoom.closeAttempted);
        // HotSpot records the absorbed failure. We do not (recorded residual in
        // W7-57-close-flush-swallow-sweep.md) — printed, not asserted.
        System.out.println("observed.printWriterCheckErrorAfterAbsorb=" + pwIo.checkError());

        BoomWriter errBoom = new BoomWriter(new Error("pw-close-boom"));
        check("printWriterClosePropagatesError",
                "java.lang.Error: pw-close-boom",
                outcome(new PrintWriter(errBoom)::close));
    }

    /**
     * `StreamHandler.close()` → `flushAndClose()` wraps the whole
     * `writer.flush(); writer.close();` in `catch (Exception ex)` and reports
     * through the `ErrorManager`. So the absorb here is WIDER than
     * `IOException` — and still not wide enough for an `Error`.
     */
    static void streamHandlerCloseIsNarrow() {
        BoomOut ioBoom = new BoomOut(BoomOut.Where.CLOSE, new IOException("sh-close-io"));
        StreamHandler shIo = new StreamHandler(ioBoom, new SimpleFormatter());
        shIo.publish(new LogRecord(Level.INFO, "hello"));
        check("streamHandlerCloseAbsorbsIOException", "none", outcome(shIo::close));

        BoomOut runtimeBoom =
                new BoomOut(BoomOut.Where.CLOSE, new IllegalStateException("sh-close-rte"));
        StreamHandler shRte = new StreamHandler(runtimeBoom, new SimpleFormatter());
        shRte.publish(new LogRecord(Level.INFO, "hello"));
        check("streamHandlerCloseAbsorbsRuntimeException", "none", outcome(shRte::close));

        BoomOut errBoom = new BoomOut(BoomOut.Where.CLOSE, new Error("sh-close-boom"));
        StreamHandler shErr = new StreamHandler(errBoom, new SimpleFormatter());
        shErr.publish(new LogRecord(Level.INFO, "hello"));
        check("streamHandlerClosePropagatesError",
                "java.lang.Error: sh-close-boom",
                outcome(shErr::close));
    }

    /**
     * A close that SUCCEEDS must stay silent — the trivial direction, kept so a
     * fix that turned every delegated close into a throw is caught here rather
     * than in a suite.
     */
    static void cleanCloseStaysClean() {
        BoomOut ok = new BoomOut(BoomOut.Where.NONE, null);
        check("cleanCloseThrowsNothing", "none", outcome(() -> {
            try (BufferedOutputStream b = new BufferedOutputStream(ok)) {
                b.write('x');
            } catch (IOException e) {
                throw new Wrapped(e);
            }
        }));
        check("cleanCloseDeliveredTheByte", 1, ok.sink.size());
        check("cleanReadThroughInputStreamReader", "none", outcome(() -> {
            try (InputStreamReader r =
                    new InputStreamReader(new ByteArrayInputStream("hi".getBytes("UTF-8")))) {
                if (r.read() != 'h') { throw new IllegalStateException("bad read"); }
            } catch (IOException e) {
                throw new Wrapped(e);
            }
        }));
    }

    static void check(String name, int expected, int actual) {
        check(name, String.valueOf(expected), String.valueOf(actual));
    }

    /** Carries a checked exception out of a lambda without renaming it. */
    static final class Wrapped extends RuntimeException {
        final Throwable real;
        Wrapped(Throwable real) { super(real); this.real = real; }
    }

    public static void main(String[] args) {
        filterOutputStreamClose();
        dataOutputStreamClose();
        inputStreamReaderClose();
        pushbackClose();
        zipAndGzipClose();
        propertiesStoreFlush();
        printWriterCloseIsNarrow();
        streamHandlerCloseIsNarrow();
        cleanCloseStaysClean();

        if (FAILURES.isEmpty()) {
            System.out.println("RESULT ok");
        } else {
            for (String f : FAILURES) {
                System.out.println("RESULT FAIL " + f);
            }
            System.out.println("RESULT FAIL count=" + FAILURES.size());
        }
    }
}
