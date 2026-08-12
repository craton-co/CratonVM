import java.io.BufferedOutputStream;
import java.io.BufferedWriter;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.io.PrintStream;
import java.io.PrintWriter;
import java.io.PushbackInputStream;
import java.io.StringWriter;
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
 * <p><b>W7-64 added the other half of the same {@code catch} clauses: what the
 * BODY does.</b> Two JDK families absorb an error and then store it somewhere
 * the caller can go and read it — {@code PrintStream}/{@code PrintWriter} set
 * {@code trouble}, which {@code checkError()} returns, and
 * {@code java.util.logging.Handler} delivers to its {@code ErrorManager}. A
 * native that absorbs but does not record has not matched the JDK; it has
 * converted a reportable failure into complete silence, and
 * {@code checkError()} then answers {@code false} forever. Every assertion in
 * that half is therefore that the failure was <b>recorded</b>, and each is
 * paired with a guard on the same call site against the opposite
 * over-correction: an {@code Error} propagates and must NOT set
 * {@code trouble}, and a healthy stream must keep answering {@code false}.
 *
 * <p>{@code checkError()} is also not a flag read — both classes open with
 * {@code if (out != null) flush();}, so a stream that has never failed but
 * whose sink cannot be flushed reports {@code true} on the first call.
 * An implementation that only reads the stored flag passes the recording
 * checks and fails {@code checkErrorFlushesBeforeItAnswers}.
 *
 * <p>Every expected value below was measured on HotSpot 25.0.3.9 (Eclipse
 * Adoptium) before it was written down; it prints {@code RESULT ok} there
 * today — 64 printed lines, all 64 asserted. (The default
 * {@code ErrorManager} writes one report and one stack trace to
 * {@code System.err} during {@code streamHandlerCloseIsNarrow}; that is
 * HotSpot's own output, not a failure.)
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
        enum Where { NONE, CLOSE, FLUSH, WRITE }

        private final Where where;
        private final Throwable what;
        final ByteArrayOutputStream sink = new ByteArrayOutputStream();
        boolean closeAttempted;
        boolean flushAttempted;

        BoomOut(Where where, Throwable what) {
            this.where = where;
            this.what = what;
        }

        @Override public void write(int b) throws IOException {
            if (where == Where.WRITE) { raise(); }
            sink.write(b);
        }

        @Override public void write(byte[] b, int off, int len) throws IOException {
            if (where == Where.WRITE) { raise(); }
            sink.write(b, off, len);
        }

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

    /**
     * A Writer whose {@code flush()} fails and whose {@code close()} does not.
     *
     * <p>Separate from {@link BoomWriter} on purpose: the checks below need a
     * writer that is healthy at construction and fails only when something
     * flushes it, which is what makes {@code checkError()}'s own flush
     * observable.
     */
    static final class FlushBoomWriter extends Writer {
        private final Throwable what;
        boolean flushAttempted;
        boolean writeAttempted;

        FlushBoomWriter(Throwable what) { this.what = what; }

        @Override public void write(char[] cbuf, int off, int len) { writeAttempted = true; }
        @Override public void write(String s, int off, int len) { writeAttempted = true; }
        @Override public void close() { }

        @Override public void flush() throws IOException {
            flushAttempted = true;
            if (what instanceof IOException) { throw (IOException) what; }
            if (what instanceof Error) { throw (Error) what; }
        }
    }

    /** A Writer whose {@code write} fails — the print/println delegation. */
    static final class WriteBoomWriter extends Writer {
        @Override public void write(char[] cbuf, int off, int len) throws IOException {
            throw new IOException("pw-write-io");
        }

        @Override public void write(String s, int off, int len) throws IOException {
            throw new IOException("pw-write-io");
        }

        @Override public void flush() { }
        @Override public void close() { }
    }

    /** Captures what {@code Handler.reportError} delivered, and nothing else. */
    static final class CapturingErrorManager extends java.util.logging.ErrorManager {
        int code = -1;
        String exception = "none";
        int calls;

        @Override public void error(String msg, Exception ex, int code) {
            this.calls++;
            this.code = code;
            this.exception = (ex == null)
                    ? "null"
                    : ex.getClass().getName() + ": " + ex.getMessage();
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
        // ASSERTED since W7-64. This row was the whole point of the residual
        // W7-57 left open: an absorbed failure that is not RECORDED is not the
        // JDK's behaviour, it is silence, and `checkError()` then answers
        // `false` forever. `true` on HotSpot 25.0.3.9.
        check("printWriterCheckErrorAfterAbsorb", true, pwIo.checkError());

        BoomWriter errBoom = new BoomWriter(new Error("pw-close-boom"));
        PrintWriter pwErr = new PrintWriter(errBoom);
        check("printWriterClosePropagatesError",
                "java.lang.Error: pw-close-boom",
                outcome(pwErr::close));
        // The over-correction guard on the RECORDING side, on the same call
        // site: an `Error` is not what `catch (IOException x)` names, so the
        // JDK's `trouble = true` never runs for it. A fix that set `trouble`
        // for every absorbed failure — or for every failure, absorbed or not —
        // would pass the row above and fail this one.
        check("printWriterCheckErrorAfterPropagatedError", false, pwErr.checkError());
    }

    /**
     * `PrintStream.flush()` is
     * `try { ensureOpen(); out.flush(); } catch (IOException x) { trouble = true; }`
     * — the same shape as `PrintWriter`'s, on a different class with a
     * different `checkError()`. Asserted separately for exactly that reason:
     * the two classes are NOT one implementation, and a repair that reaches
     * only the one whose native was edited looks identical to a repair that
     * reached both.
     */
    static void printStreamRecordsWhatItAbsorbs() {
        BoomOut flushIo = new BoomOut(BoomOut.Where.FLUSH, new IOException("ps-flush-io"));
        PrintStream psIo = new PrintStream(flushIo);
        check("printStreamFlushAbsorbsIOException", "none", outcome(psIo::flush));
        check("printStreamFlushWasAttemptedDespiteAbsorb", true, flushIo.flushAttempted);
        check("printStreamCheckErrorAfterAbsorbedFlush", true, psIo.checkError());

        // The write path records too, and through a different JDK body: every
        // `print`/`println` overload funnels into a private `write`/`writeln`
        // that ends `catch (IOException x) { trouble = true; }`.
        BoomOut printIo = new BoomOut(BoomOut.Where.WRITE, new IOException("ps-write-io"));
        PrintStream psPrint = new PrintStream(printIo);
        check("printStreamPrintAbsorbsIOException", "none", outcome(() -> psPrint.print("hello")));
        check("printStreamCheckErrorAfterFailedPrint", true, psPrint.checkError());

        BoomOut printlnIo = new BoomOut(BoomOut.Where.WRITE, new IOException("ps-writeln-io"));
        PrintStream psPrintln = new PrintStream(printlnIo);
        check("printStreamPrintlnAbsorbsIOException", "none",
                outcome(() -> psPrintln.println("hello")));
        check("printStreamCheckErrorAfterFailedPrintln", true, psPrintln.checkError());

        BoomOut writeIo = new BoomOut(BoomOut.Where.WRITE, new IOException("ps-write-int-io"));
        PrintStream psWrite = new PrintStream(writeIo);
        check("printStreamWriteIntAbsorbsIOException", "none",
                outcome(() -> psWrite.write('x')));
        check("printStreamCheckErrorAfterFailedWriteInt", true, psWrite.checkError());

        // `PrintWriter`'s write path, same argument.
        PrintWriter pwWrite = new PrintWriter(new WriteBoomWriter());
        check("printWriterPrintAbsorbsIOException", "none", outcome(() -> pwWrite.print("hello")));
        check("printWriterCheckErrorAfterFailedPrint", true, pwWrite.checkError());
    }

    /**
     * `checkError()` is not a flag read. Both classes open with
     * `if (out != null) flush();`, so a stream that has never failed but whose
     * sink cannot be flushed reports `true` on the FIRST call — and the sink
     * records that the flush was attempted.
     *
     * <p>An implementation that only reads the stored flag passes every check
     * above and fails these, which is why they are here: the read side is half
     * the contract and it is the half a `trouble`-only repair loses.
     */
    static void checkErrorFlushesBeforeItAnswers() {
        BoomOut psSink = new BoomOut(BoomOut.Where.FLUSH, new IOException("ps-ck-flush"));
        PrintStream ps = new PrintStream(psSink);
        check("printStreamFlushNotYetAttempted", false, psSink.flushAttempted);
        check("printStreamCheckErrorItselfFlushed", true, ps.checkError());
        check("printStreamFlushAttemptedByCheckError", true, psSink.flushAttempted);

        FlushBoomWriter pwSink = new FlushBoomWriter(new IOException("pw-ck-flush"));
        PrintWriter pw = new PrintWriter(pwSink);
        check("printWriterFlushNotYetAttempted", false, pwSink.flushAttempted);
        check("printWriterCheckErrorItselfFlushed", true, pw.checkError());
        check("printWriterFlushAttemptedByCheckError", true, pwSink.flushAttempted);

        // `PrintWriter.checkError()` delegates: `else if (psOut != null) return
        // psOut.checkError();`. The inner stream's `trouble` is what the outer
        // writer reports, so a per-object flag that is never consulted through
        // the delegation answers `false` here.
        BoomOut inner = new BoomOut(BoomOut.Where.FLUSH, new IOException("delegate-io"));
        PrintStream innerPs = new PrintStream(inner);
        PrintWriter outerPw = new PrintWriter(innerPs);
        innerPs.flush();
        check("innerPrintStreamCheckError", true, innerPs.checkError());
        check("printWriterCheckErrorDelegatesToPrintStream", true, outerPw.checkError());
    }

    /**
     * The over-correction guards for the recording side, on healthy streams.
     *
     * <p>A repair that sets `trouble` unconditionally — on every delegated
     * call, or on a clean return — makes every one of the assertions above pass
     * and turns `checkError()` into a constant `true`, which is the same defect
     * with the sign flipped: a caller that checks it now aborts on a stream
     * that never failed.
     */
    static void healthyStreamsReportNoError() {
        BoomOut ok = new BoomOut(BoomOut.Where.NONE, null);
        PrintStream ps = new PrintStream(ok);
        ps.print("ok");
        ps.println("ok");
        ps.flush();
        check("printStreamCheckErrorOnHealthyStream", false, ps.checkError());
        check("printStreamHealthyStreamGotItsBytes", true, ok.sink.size() > 0);

        StringWriter sw = new StringWriter();
        PrintWriter pw = new PrintWriter(sw);
        pw.print("ok");
        pw.println("ok");
        pw.flush();
        check("printWriterCheckErrorOnHealthyWriter", false, pw.checkError());
        check("printWriterHealthyWriterGotItsText", true, sw.toString().length() > 0);

        // A CLEAN close leaves no error either. HotSpot reaches `false` here
        // by a different route than we do — it nulls `out`, so `checkError()`
        // skips its flush entirely — but the answer is the observable, and it
        // is `false` on both.
        PrintWriter closed = new PrintWriter(new StringWriter());
        closed.close();
        check("printWriterCheckErrorAfterCleanClose", false, closed.checkError());
    }

    /**
     * The JDK's `protected` write side. `setError()` exists so a `PrintStream`
     * subclass can report a failure of its own, and `clearError()` so it can
     * take it back; both are specified in terms of what `checkError()` returns
     * afterwards, so they are the same field seen from the other end.
     */
    static void setErrorAndClearErrorMoveTheSameFlag() {
        class SubPs extends PrintStream {
            SubPs(OutputStream out) { super(out); }
            void raise() { setError(); }
            void reset() { clearError(); }
        }
        SubPs ps = new SubPs(new ByteArrayOutputStream());
        check("subclassPrintStreamCheckErrorInitially", false, ps.checkError());
        ps.raise();
        check("subclassPrintStreamCheckErrorAfterSetError", true, ps.checkError());
        ps.reset();
        check("subclassPrintStreamCheckErrorAfterClearError", false, ps.checkError());

        class SubPw extends PrintWriter {
            SubPw(Writer out) { super(out); }
            void raise() { setError(); }
            void reset() { clearError(); }
        }
        SubPw pw = new SubPw(new StringWriter());
        check("subclassPrintWriterCheckErrorInitially", false, pw.checkError());
        pw.raise();
        check("subclassPrintWriterCheckErrorAfterSetError", true, pw.checkError());
        pw.reset();
        check("subclassPrintWriterCheckErrorAfterClearError", false, pw.checkError());
    }

    /**
     * `java.util.logging.Handler` is the same species with a different
     * destination: `catch (Exception ex) { reportError(null, ex,
     * ErrorManager.<CODE>); }`. The absorbed exception is not discarded, it is
     * DELIVERED — so an implementation that absorbs and drops has again turned
     * a reportable failure into silence.
     *
     * <p>The codes are compared against literals rather than
     * `ErrorManager.FLUSH_FAILURE` on purpose: those constants are
     * `public static final int` on a class a synthetic-JDK build fabricates
     * without a static field table, so reading them would make this check red
     * for a reason that is not the one being probed. `2` and `3` are what
     * HotSpot 25.0.3.9 passed.
     */
    static void streamHandlerReachesItsErrorManager() {
        BoomOut flushIo = new BoomOut(BoomOut.Where.FLUSH, new IOException("sh-flush-io"));
        StreamHandler shFlush = new StreamHandler(flushIo, new SimpleFormatter());
        CapturingErrorManager emFlush = new CapturingErrorManager();
        shFlush.setErrorManager(emFlush);
        shFlush.publish(new LogRecord(Level.INFO, "hello"));
        check("streamHandlerFlushAbsorbsIOException", "none", outcome(shFlush::flush));
        check("streamHandlerFlushReportedToErrorManager", 2, emFlush.code);
        check("streamHandlerFlushReportedTheException",
                "java.io.IOException: sh-flush-io", emFlush.exception);

        BoomOut closeIo = new BoomOut(BoomOut.Where.CLOSE, new IOException("sh-close-io"));
        StreamHandler shClose = new StreamHandler(closeIo, new SimpleFormatter());
        CapturingErrorManager emClose = new CapturingErrorManager();
        shClose.setErrorManager(emClose);
        shClose.publish(new LogRecord(Level.INFO, "hello"));
        check("streamHandlerCloseAbsorbsIOExceptionAgain", "none", outcome(shClose::close));
        check("streamHandlerCloseReportedToErrorManager", 3, emClose.code);
        check("streamHandlerCloseReportedTheException",
                "java.io.IOException: sh-close-io", emClose.exception);

        // The over-correction guard, on the same call site: `catch (Exception)`
        // does not name an `Error`, so the `ErrorManager` must NOT hear about
        // one — it is not a handler failure, it propagates.
        BoomOut closeErr = new BoomOut(BoomOut.Where.CLOSE, new Error("sh-close-boom"));
        StreamHandler shErr = new StreamHandler(closeErr, new SimpleFormatter());
        CapturingErrorManager emErr = new CapturingErrorManager();
        shErr.setErrorManager(emErr);
        shErr.publish(new LogRecord(Level.INFO, "hello"));
        check("streamHandlerClosePropagatesErrorAgain",
                "java.lang.Error: sh-close-boom", outcome(shErr::close));
        check("streamHandlerErrorManagerNotCalledForError", 0, emErr.calls);

        // A `Handler` always has an `ErrorManager`: the JDK's field initializer
        // is `= new ErrorManager()` and its own javadoc promises a default is
        // installed. A native `<init>` that reconstructs only some of a class's
        // field initializers leaves this null, and every `reportError` on that
        // handler then NPEs inside the reporting path.
        check("streamHandlerHasADefaultErrorManager",
                true, new StreamHandler().getErrorManager() != null);
        // And a healthy handler reports nothing at all.
        BoomOut healthy = new BoomOut(BoomOut.Where.NONE, null);
        StreamHandler shOk = new StreamHandler(healthy, new SimpleFormatter());
        CapturingErrorManager emOk = new CapturingErrorManager();
        shOk.setErrorManager(emOk);
        shOk.publish(new LogRecord(Level.INFO, "hello"));
        shOk.flush();
        shOk.close();
        check("streamHandlerHealthyReportsNothing", 0, emOk.calls);
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
        // W7-64 — the RECORDING half of the same `catch` clauses.
        printStreamRecordsWhatItAbsorbs();
        checkErrorFlushesBeforeItAnswers();
        healthyStreamsReportNoError();
        setErrorAndClearErrorMoveTheSameFlag();
        streamHandlerReachesItsErrorManager();

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
