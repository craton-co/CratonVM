import java.io.BufferedOutputStream;
import java.io.BufferedWriter;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
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
 * <p><b>W7-81 added the WRITE path's routing answer.</b> A delegated write has
 * three outcomes and not two — delivered, absorbed (the JDK's
 * {@code catch (IOException x)} ran and the bytes are gone), refused (an
 * {@code Error} the JDK's {@code catch} does not name) — and the byte sink and
 * the char sink must give the same answer to each. Those rows assert where the
 * characters ended up and what the receiver recorded, on both sinks, for all
 * three; and the {@code Error} row's {@code checkError()} is the guard on the
 * console fallback that the refusal answer keeps alive. The fallback itself
 * writes to a raw file descriptor and is therefore invisible from inside the
 * JVM — see {@link #consoleEchoIsOutOfBand()}, which collects that evidence and
 * says plainly that it cannot judge it.
 *
 * <p>Every expected value below was measured on HotSpot 25.0.3.9 (Eclipse
 * Adoptium) before it was written down; it prints {@code RESULT ok} there
 * today — 119 printed lines, 112 of them asserted. (The default
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

    /**
     * W7-70 — an {@code OutputStream} that RECORDS the order it was driven in.
     *
     * <p>Separate from {@link BoomOut}, which records only "was it attempted".
     * {@code PrintStream.close()} drives its sink twice, and which of the two
     * calls happened — and in which order, and whether the second happened at
     * all — is the whole content of the contract being probed. {@code BoomOut}
     * cannot express "flush ran and close did not".
     *
     * <p>The {@code write} ops are recorded but never asserted on: this VM's
     * {@code println} writes the text and the line separator as ONE buffer and
     * uses {@code "\n"} where HotSpot on Windows uses {@code "\r\n"}, so the
     * byte counts legitimately differ. {@link #flushCloseTrace()} projects the
     * trace down to just the flush/close sequence, which is the part the JDK
     * fixes.
     */
    static final class TraceOut extends OutputStream {
        final List<String> ops = new ArrayList<>();
        final ByteArrayOutputStream sink = new ByteArrayOutputStream();
        private final Throwable flushBoom;
        private final Throwable closeBoom;

        TraceOut() { this(null, null); }

        TraceOut(Throwable flushBoom, Throwable closeBoom) {
            this.flushBoom = flushBoom;
            this.closeBoom = closeBoom;
        }

        @Override public void write(int b) { ops.add("write"); sink.write(b); }

        @Override public void write(byte[] b, int off, int len) {
            ops.add("write");
            sink.write(b, off, len);
        }

        @Override public void flush() throws IOException {
            ops.add("flush");
            raise(flushBoom);
        }

        @Override public void close() throws IOException {
            ops.add("close");
            raise(closeBoom);
        }

        private static void raise(Throwable what) throws IOException {
            if (what == null) { return; }
            if (what instanceof IOException) { throw (IOException) what; }
            if (what instanceof RuntimeException) { throw (RuntimeException) what; }
            if (what instanceof Error) { throw (Error) what; }
        }

        /** The trace with every {@code write} dropped, joined with commas. */
        String flushCloseTrace() {
            StringBuilder sb = new StringBuilder();
            for (String op : ops) {
                if (op.equals("write")) { continue; }
                if (sb.length() > 0) { sb.append(','); }
                sb.append(op);
            }
            return sb.toString();
        }

        int count(String op) {
            int n = 0;
            for (String o : ops) { if (o.equals(op)) { n++; } }
            return n;
        }
    }

    /**
     * W7-81 — a {@code Writer} that records what reached it and can fail on
     * {@code write} with a chosen throwable.
     *
     * <p>The CHAR twin of {@link TraceOut}. {@link WriteBoomWriter} always
     * throws and keeps nothing, which cannot express "the sink received
     * exactly these characters and nothing more" — and "how much reached the
     * sink" is half of what the three-way routing answer decides.
     */
    static final class TraceWriter extends Writer {
        final StringBuilder received = new StringBuilder();
        private final Throwable writeBoom;

        TraceWriter() { this(null); }

        TraceWriter(Throwable writeBoom) { this.writeBoom = writeBoom; }

        @Override public void write(char[] cbuf, int off, int len) throws IOException {
            raise();
            received.append(cbuf, off, len);
        }

        @Override public void write(String s, int off, int len) throws IOException {
            raise();
            received.append(s, off, off + len);
        }

        @Override public void flush() { }
        @Override public void close() { }

        private void raise() throws IOException {
            if (writeBoom == null) { return; }
            if (writeBoom instanceof IOException) { throw (IOException) writeBoom; }
            if (writeBoom instanceof RuntimeException) { throw (RuntimeException) writeBoom; }
            if (writeBoom instanceof Error) { throw (Error) writeBoom; }
        }
    }

    /**
     * W7-81 — a failing byte sink whose slot 0 is {@code System.out}.
     *
     * <p>Exists only to make the console fallback REACHABLE. This VM's
     * {@code stream_fd} decides "is this a console stream" by walking field 0
     * up to four hops looking for pointer-identity with {@code System.out} /
     * {@code System.err}; a sink whose fields lead nowhere makes "not routed"
     * indistinguishable from "routed", because the fallback then has no fd to
     * write to and does nothing either way. {@code OutputStream} declares no
     * instance fields, so {@code chain} is slot 0.
     *
     * <p>On HotSpot the class is inert scaffolding: nothing there walks slots.
     */
    static final class EchoBoomOut extends OutputStream {
        final OutputStream chain;
        private final Throwable writeBoom;

        EchoBoomOut(OutputStream chain, Throwable writeBoom) {
            this.chain = chain;
            this.writeBoom = writeBoom;
        }

        @Override public void write(int b) throws IOException { raise(); }

        @Override public void write(byte[] b, int off, int len) throws IOException { raise(); }

        @Override public void flush() { }
        @Override public void close() { }

        private void raise() throws IOException {
            if (writeBoom instanceof IOException) { throw (IOException) writeBoom; }
            if (writeBoom instanceof RuntimeException) { throw (RuntimeException) writeBoom; }
            if (writeBoom instanceof Error) { throw (Error) writeBoom; }
        }
    }

    /**
     * W7-81 — the CHAR twin of {@link EchoBoomOut}.
     *
     * <p>{@code Writer}'s {@code protected Writer(Object lock)} constructor is
     * what puts {@code System.out} in slot 0 here; a {@code PrintWriter} over
     * this writer therefore has {@code System.out} two hops down its slot-0
     * chain, which is the picocli / JUnit-console shape.
     */
    static final class EchoBoomWriter extends Writer {
        private final Throwable writeBoom;

        EchoBoomWriter(Object chain, Throwable writeBoom) {
            super(chain);
            this.writeBoom = writeBoom;
        }

        @Override public void write(char[] cbuf, int off, int len) throws IOException { raise(); }

        @Override public void write(String s, int off, int len) throws IOException { raise(); }

        @Override public void flush() { }
        @Override public void close() { }

        private void raise() throws IOException {
            if (writeBoom instanceof IOException) { throw (IOException) writeBoom; }
            if (writeBoom instanceof RuntimeException) { throw (RuntimeException) writeBoom; }
            if (writeBoom instanceof Error) { throw (Error) writeBoom; }
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

    // ------------------------------------------------------- W7-70: close()

    /** Reads a whole file back as a UTF-8 String, with no java.nio.file. */
    static String slurp(File f) throws IOException {
        try (FileInputStream in = new FileInputStream(f)) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[512];
            int n;
            while ((n = in.read(buf)) > 0) { out.write(buf, 0, n); }
            return out.toString("UTF-8");
        }
    }

    /**
     * W7-70 — {@code PrintStream.close()} must deliver the bytes and release
     * the sink.
     *
     * <p>The defect: {@code native_printstream_close} was a bare no-op for
     * EVERY {@code PrintStream}, not only the console ones its comment named.
     * So the assertions here are that the effect <b>arrives</b> — bytes are on
     * disk, the sink's {@code close()} ran — never that the call returned. A
     * no-op {@code close()} returns perfectly cleanly; that is the whole
     * problem with it.
     *
     * <p>The buffered wrapper is load-bearing. A {@code PrintStream} straight
     * over a {@code FileOutputStream} has its bytes on disk before
     * {@code close()} is ever called, so the file would be complete even with
     * the no-op and the check would pass for the wrong reason.
     */
    static void printStreamCloseDeliversAndReleases() throws IOException {
        File f = File.createTempFile("w770-buffered", ".txt");
        f.deleteOnExit();
        PrintStream ps = new PrintStream(new BufferedOutputStream(new FileOutputStream(f)));
        ps.println("data-on-disk");
        // Nothing has reached the file yet: the BufferedOutputStream holds it.
        check("printStreamFileEmptyBeforeClose", true, f.length() == 0);
        ps.close();
        check("printStreamFileNonEmptyAfterClose", true, f.length() > 0);
        // Trimmed, because this VM's line separator is "\n" where HotSpot's on
        // Windows is "\r\n" — the separator is not what is being probed.
        check("printStreamFileContentAfterClose", "data-on-disk", slurp(f).trim());
        // OVER-CORRECTION GUARD, same call site: a clean close records nothing.
        check("printStreamCheckErrorAfterCleanFileClose", false, ps.checkError());

        // The sink-level proof that the handle-releasing call was made. This is
        // the platform-free half; the delete rows below are the Windows-only
        // corroboration.
        TraceOut trace = new TraceOut();
        PrintStream sinkPs = new PrintStream(trace);
        sinkPs.print("q");
        check("printStreamSinkNotClosedBeforeClose", 0, trace.count("close"));
        sinkPs.close();
        check("printStreamSinkClosedAfterClose", 1, trace.count("close"));
        check("printStreamCloseDeliveredEveryByte", "q", trace.sink.toString("UTF-8"));

        // The OS-level handle. On Windows an open handle blocks delete; on
        // Linux it does not, so the expectation is derived from the platform
        // rather than fixed. Both halves are real assertions on Windows; on
        // Linux the first asserts that delete is NOT blocked and the second is
        // vacuous — which is why the sink rows above exist and this pair is
        // not the only evidence.
        boolean windows = System.getProperty("os.name", "").toLowerCase().contains("win");
        File h = File.createTempFile("w770-handle", ".txt");
        h.deleteOnExit();
        PrintStream hps = new PrintStream(new FileOutputStream(h));
        hps.println("x");
        boolean deletedWhileOpen = h.delete();
        check("printStreamDeleteBlockedWhileOpen", windows, !deletedWhileOpen);
        hps.close();
        check("printStreamDeleteSucceedsAfterClose", true, !h.exists() || h.delete());
    }

    /**
     * W7-70 — the ORDER {@code close()} drives the sink in, and what a failure
     * at each step does.
     *
     * <p>Measured on HotSpot 25.0.3.9, not read off {@code PrintStream.close}'s
     * source, because that source does not look like it flushes and it does:
     * {@code charOut} is {@code new OutputStreamWriter(this, charset)}, so
     * {@code textOut.close()} bottoms out in {@code StreamEncoder.implClose},
     * whose {@code out} is {@code this} — it calls {@code this.flush()}, which
     * is {@code out.flush()} on the real sink, and then {@code this.close()},
     * which the {@code closing} latch turns into a no-op. The sink sees
     * {@code flush} then {@code close}.
     */
    static void printStreamCloseOrdering() {
        TraceOut clean = new TraceOut();
        PrintStream p1 = new PrintStream(clean);
        p1.println("hi");
        check("printStreamCleanCloseThrowsNothing", "none", outcome(p1::close));
        check("printStreamCleanCloseSinkTrace", "flush,close", clean.flushCloseTrace());

        // A sink flush that raises an IOException is ABSORBED — `close()`
        // declares no checked exception — and the close still runs.
        TraceOut flushIo = new TraceOut(new IOException("ps-flush-io"), null);
        PrintStream p2 = new PrintStream(flushIo);
        check("printStreamCloseAbsorbsSinkFlushIOException", "none", outcome(p2::close));
        check("printStreamFlushIoSinkTrace", "flush,close", flushIo.flushCloseTrace());
        check("printStreamCheckErrorAfterAbsorbedFlushOnClose", true, p2.checkError());

        // …but an Error is not what that `catch` names, and it PROPAGATES —
        // and because the two statements are straight-line inside one `try`,
        // the close is SKIPPED. This is the row a repair spelled
        // "flush; close;" with the failure dropped between them gets wrong.
        TraceOut flushErr = new TraceOut(new Error("ps-flush-boom"), null);
        PrintStream p3 = new PrintStream(flushErr);
        check("printStreamClosePropagatesSinkFlushError",
                "java.lang.Error: ps-flush-boom", outcome(p3::close));
        check("printStreamFlushErrorSkipsTheClose", "flush", flushErr.flushCloseTrace());

        // A sink close that raises an IOException is absorbed and RECORDED.
        // This is the row W7-64 could not write, because the call this VM made
        // there was not the call HotSpot makes.
        TraceOut closeIo = new TraceOut(null, new IOException("ps-close-io"));
        PrintStream p4 = new PrintStream(closeIo);
        check("printStreamCloseAbsorbsSinkCloseIOException", "none", outcome(p4::close));
        check("printStreamCheckErrorAfterAbsorbedClose", true, p4.checkError());
        check("printStreamCloseIoSinkTrace", "flush,close", closeIo.flushCloseTrace());

        // OVER-CORRECTION GUARD, same call site: an Error propagates and must
        // NOT set `trouble`. HotSpot's `catch` never sees it, so a `trouble`
        // set here would be invented state, not parity.
        TraceOut closeErr = new TraceOut(null, new Error("ps-close-boom"));
        PrintStream p5 = new PrintStream(closeErr);
        check("printStreamClosePropagatesSinkCloseError",
                "java.lang.Error: ps-close-boom", outcome(p5::close));
        check("printStreamCheckErrorAfterPropagatedCloseError", false, p5.checkError());
    }

    /**
     * W7-70 — a second {@code close()} is a TOTAL no-op.
     *
     * <p>HotSpot's {@code closing} latch is never cleared, so the second call
     * does not reach the sink, throws nothing, and does not move
     * {@code trouble}. This is the over-correction guard for the whole lane:
     * a {@code PrintStream} that starts throwing from {@code close()}, or that
     * re-closes its sink, is a worse defect than one that fails to close.
     */
    static void printStreamDoubleCloseIsHarmless() {
        TraceOut trace = new TraceOut();
        PrintStream ps = new PrintStream(trace);
        ps.println("dc");
        ps.close();
        check("printStreamDoubleCloseThrowsNothing", "none", outcome(ps::close));
        check("printStreamDoubleCloseDidNotRecloseSink", 1, trace.count("close"));
        check("printStreamDoubleCloseDidNotReflushSink", 1, trace.count("flush"));
        check("printStreamCheckErrorAfterDoubleClose", false, ps.checkError());

        // And a close that PROPAGATED still latched: the JDK sets `closing`
        // before the delegation and never clears it, so the retry after an
        // Error is a no-op too.
        TraceOut boom = new TraceOut(null, new Error("dc-boom"));
        PrintStream bps = new PrintStream(boom);
        check("printStreamFirstCloseRaised", "java.lang.Error: dc-boom", outcome(bps::close));
        check("printStreamRetryAfterPropagatedCloseThrowsNothing", "none", outcome(bps::close));
        check("printStreamRetryAfterPropagatedCloseDidNotRetouchSink", 1, boom.count("close"));

        // A closed stream's `checkError()` does not re-flush: HotSpot reaches
        // that by nulling `out`, so its `if (out != null) flush()` guard skips.
        // The ANSWER is asserted; the flush COUNT is only observed, and the
        // split is per-arm rather than cosmetic.
        //
        // Under `--synthetic-jdk` the `checkError` native reads the `closing`
        // latch and skips its flush, so the count holds. In Compatible mode
        // `checkError()` is real `java.io` bytecode reading a real, non-null
        // `out` — `native_printstream_close` cannot null it, because a null
        // `out` is this VM's "console stream" marker — so it flushes a closed
        // sink every call. The answer is the same either way, because the
        // close already recorded; only the extra flush differs. Asserting the
        // count would make the probe red in Compatible mode for a residual
        // this lane names rather than fixes.
        // W7-70-printstream-close-noop.md
        TraceOut flushOnly = new TraceOut(new IOException("late-flush-io"), null);
        PrintStream fps = new PrintStream(flushOnly);
        fps.close();
        check("printStreamCheckErrorAfterCloseOverFlushBoomSink", true, fps.checkError());
        int flushesAfterFirstCheck = flushOnly.count("flush");
        fps.checkError();
        System.out.println("observed.printStreamCheckErrorOnClosedStreamReflushed="
                + (flushOnly.count("flush") - flushesAfterFirstCheck));
    }

    /**
     * W7-70 — the named residual, printed and NOT asserted.
     *
     * <p>HotSpot's {@code close()} nulls {@code out}, so a write afterwards
     * fails {@code ensureOpen()}, delivers nothing and sets {@code trouble}.
     * This VM deliberately does not null {@code out} — a null {@code out} is
     * its "this is a console stream" marker, and nulling it would redirect a
     * closed stream's output to stdout — so a post-close write reaches the
     * closed sink instead of being refused at the door. Asserting this would
     * make the probe red for something this lane did not claim to fix; it is
     * printed so the next lane can see the gap move. Measured on HotSpot
     * 25.0.3.9: {@code 0} and {@code true}.
     */
    static void printStreamWriteAfterCloseObservation() throws IOException {
        TraceOut trace = new TraceOut();
        PrintStream ps = new PrintStream(trace);
        ps.close();
        int before = trace.sink.size();
        ps.println("after-close");
        System.out.println("observed.printStreamBytesWrittenAfterClose="
                + (trace.sink.size() - before));
        System.out.println("observed.printStreamCheckErrorAfterWriteOnClosedStream="
                + ps.checkError());
    }

    /**
     * W7-81 — a delegated WRITE has three outcomes, not two, and the byte sink
     * and the char sink must give the same answer to each.
     *
     * <p>The routing helper behind every {@code print}/{@code println} native
     * used to answer one {@code bool} that meant two different things in its
     * two branches. Its char branch reported an ABSORBED {@code IOException} as
     * "the write did not happen", which sent the text to the console fast path
     * — a second write, to a stream HotSpot never touched, because HotSpot's
     * {@code catch (IOException x) { trouble = true; }} discards the bytes. Its
     * byte branch reported EVERY failure as "the write happened", including an
     * {@code Error}, so a {@code NoSuchMethodError} from our own dispatch made
     * the text vanish with no fallback at all.
     *
     * <p>So the contract each row below asserts is <b>where the characters
     * ended up and what the receiver recorded</b>, on both sinks, for all three
     * outcomes:
     *
     * <table><tr><th>sink raised</th><th>reached the sink</th>
     * <th>{@code checkError()}</th><th>thrown at the caller</th></tr>
     * <tr><td>nothing</td><td>everything</td><td>{@code false}</td><td>none</td></tr>
     * <tr><td>{@code IOException}</td><td>nothing</td><td>{@code true}</td><td>none</td></tr>
     * <tr><td>{@code Error}</td><td>nothing</td><td>{@code false}</td><td>the {@code Error}</td></tr>
     * </table>
     *
     * <p><b>The {@code Error} row's {@code checkError()} is the guard that
     * keeps the console fallback alive.</b> The tempting way to collapse the
     * three answers back into two is to widen "absorbed" to cover an
     * {@code Error} as well — which is also exactly how the picocli /
     * JUnit-console survival path gets deleted, because "absorbed" means
     * "routed" means "do not fall back". Widening it sets {@code trouble} for a
     * failure HotSpot's {@code catch} never sees, so this row goes red the
     * moment the fallback is removed that way. It is the only in-process
     * observable that moves with it; see {@link #consoleEchoIsOutOfBand()}.
     */
    static void writeRoutingIsThreeWay() {
        // ---- byte sink (PrintStream over an OutputStream) ----
        TraceOut cleanBytes = new TraceOut();
        PrintStream psClean = new PrintStream(cleanBytes);
        check("psRouteCleanWriteThrewNothing", "none", outcome(() -> psClean.print("hello")));
        check("psRouteCleanWriteReachedSink", "hello", cleanBytes.sink.toString());
        check("psRouteCleanWriteNoTrouble", false, psClean.checkError());

        BoomOut ioBytes = new BoomOut(BoomOut.Where.WRITE, new IOException("route-ps-io"));
        PrintStream psIo = new PrintStream(ioBytes);
        check("psRouteIoWriteThrewNothing", "none", outcome(() -> psIo.print("hello")));
        // HotSpot's `catch` discards the bytes: they are on no stream anywhere.
        // A fallback that re-writes them puts them on a console HotSpot left
        // untouched, which is a DOUBLE write, not a rescue.
        check("psRouteIoWriteReachedSinkBytes", 0, ioBytes.sink.size());
        check("psRouteIoWriteRecordedTrouble", true, psIo.checkError());

        BoomOut errBytes = new BoomOut(BoomOut.Where.WRITE, new Error("route-ps-boom"));
        PrintStream psErr = new PrintStream(errBytes);
        String psErrOutcome = outcome(() -> psErr.print("hello"));
        check("psRouteErrorWriteReachedSinkBytes", 0, errBytes.sink.size());
        // The fallback guard. `catch (IOException x)` does not name an `Error`,
        // so `trouble` must stay clear — and staying clear is what "refused,
        // fall back to the console" looks like from inside the JVM.
        check("psRouteErrorWriteDidNotRecordTrouble", false, psErr.checkError());

        // ---- char sink (PrintWriter over a Writer) ----
        TraceWriter cleanChars = new TraceWriter();
        PrintWriter pwClean = new PrintWriter(cleanChars);
        check("pwRouteCleanWriteThrewNothing", "none", outcome(() -> pwClean.print("hello")));
        check("pwRouteCleanWriteReachedSink", "hello", cleanChars.received.toString());
        check("pwRouteCleanWriteNoTrouble", false, pwClean.checkError());

        TraceWriter ioChars = new TraceWriter(new IOException("route-pw-io"));
        PrintWriter pwIo = new PrintWriter(ioChars);
        check("pwRouteIoWriteThrewNothing", "none", outcome(() -> pwIo.print("hello")));
        check("pwRouteIoWriteReachedSinkChars", 0, ioChars.received.length());
        check("pwRouteIoWriteRecordedTrouble", true, pwIo.checkError());

        TraceWriter errChars = new TraceWriter(new Error("route-pw-boom"));
        PrintWriter pwErr = new PrintWriter(errChars);
        String pwErrOutcome = outcome(() -> pwErr.print("hello"));
        check("pwRouteErrorWriteReachedSinkChars", 0, errChars.received.length());
        check("pwRouteErrorWriteDidNotRecordTrouble", false, pwErr.checkError());

        // ---- the two branches must give the SAME answer ----
        // Asserted as an explicit equality rather than left implicit in the
        // twelve rows above: "one branch was fixed" and "both branches were
        // fixed" look identical row by row, and the disagreement between them
        // is the defect this section exists for.
        check("routeBranchesAgreeOnCleanDelivery",
                cleanBytes.sink.toString(), cleanChars.received.toString());
        check("routeBranchesAgreeOnIoException",
                ioBytes.sink.size() + "/" + psIo.checkError(),
                ioChars.received.length() + "/" + pwIo.checkError());
        check("routeBranchesAgreeOnError",
                errBytes.sink.size() + "/" + psErr.checkError(),
                errChars.received.length() + "/" + pwErr.checkError());

        // PRINTED, NOT ASSERTED — mode-dependent, and the reason is named.
        // HotSpot lets an `Error` out of `print`; this VM's write natives return
        // `void` through helpers that cannot propagate, and W7-70 established
        // that making them propagate deletes the console fallback on the exact
        // case (a `NoSuchMethodError` from our own dispatch) it exists for. So
        // "none" here is a KEPT divergence, not a regression, and asserting the
        // HotSpot value would demand the change this lane declined to make.
        // Measured on HotSpot 25.0.3.9: both are the `Error`.
        // W7-70-printstream-close-noop.md, W7-81-write-route-three-way.md
        System.out.println("observed.psRouteErrorWriteOutcome=" + psErrOutcome);
        System.out.println("observed.pwRouteErrorWriteOutcome=" + pwErrOutcome);
    }

    /**
     * W7-81 — the console echo itself, which NO check in this file can assert.
     *
     * <p>The whole behaviour delta of the three-way routing answer lands on one
     * thing: whether the caller falls back to the console file descriptor.
     * That fallback is a raw host write to fd 1 — it does not go through
     * {@code System.out}, so {@code System.setOut} cannot capture it and no
     * code running inside the JVM can see it. That is precisely why W7-70 wrote
     * the design down and did not ship it: it "could not measure".
     *
     * <p>This method does not pretend otherwise. It performs the two writes
     * whose echo the change moves, tagged with tokens, and prints what the
     * console should and should not contain. Whoever runs the probe reads the
     * process's stdout; the tokens are the evidence, and they are evidence the
     * probe collects but cannot judge.
     *
     * <table><tr><th>token</th><th>before</th><th>after</th><th>HotSpot</th></tr>
     * <tr><td>{@code W781-IO-MUST-NOT-ECHO}</td><td>echoed (char branch
     * reported the absorbed {@code IOException} as "not routed")</td>
     * <td>absent</td><td>absent</td></tr>
     * <tr><td>{@code W781-ERR-MUST-ECHO}</td><td>absent (byte branch reported
     * the {@code Error} as "routed" and the text vanished)</td><td>echoed</td>
     * <td>absent — HotSpot throws instead, which is the one outcome this VM
     * cannot offer</td></tr></table>
     *
     * <p>The two sinks chain {@code System.out} through slot 0 on purpose: the
     * fallback picks its fd by walking that chain, so a sink whose fields lead
     * nowhere makes "not routed" do nothing and hides the very difference being
     * looked for. That construction depends on this VM's field layout rather
     * than on any Java contract, which is a second reason nothing here is
     * asserted.
     */
    static void consoleEchoIsOutOfBand() {
        System.out.println("observed.echoTokenThatMustNotAppear=W781-IO-MUST-NOT-ECHO");
        System.out.println("observed.echoTokenThatMustAppearOnCratonVM=W781-ERR-MUST-ECHO");

        PrintWriter pwIo =
                new PrintWriter(new EchoBoomWriter(System.out, new IOException("echo-pw-io")));
        pwIo.print("W781-IO-MUST-NOT-ECHO");
        pwIo.flush();

        PrintStream psErr =
                new PrintStream(new EchoBoomOut(System.out, new Error("echo-ps-boom")));
        try {
            psErr.print("W781-ERR-MUST-ECHO");
        } catch (Error expectedOnHotSpot) {
            // HotSpot propagates it here and echoes nothing. Swallowed so the
            // probe still reaches `RESULT ok` on the reference JVM.
        }
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

    public static void main(String[] args) throws IOException {
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
        // W7-70 — close() itself, which was a no-op for every PrintStream.
        printStreamCloseDeliversAndReleases();
        printStreamCloseOrdering();
        printStreamDoubleCloseIsHarmless();
        printStreamWriteAfterCloseObservation();
        // W7-81 — the WRITE path's routing answer, and the two sinks agreeing.
        writeRoutingIsThreeWay();
        consoleEchoIsOutOfBand();

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
