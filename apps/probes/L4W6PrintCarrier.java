import java.io.*;
import java.lang.reflect.*;
import java.nio.charset.*;

/** L4 wave 6 -- the `java.io.Print*` CARRIER, asked before anything is retired.
 *
 *  Two populations share one registrar
 *  (`register_printstream_fallback_natives`) and got opposite verdicts on
 *  2026-08-11: `java/io/PrintWriter`'s seven rows RETIRABLE, `java/io/
 *  PrintStream`'s twenty-nine BLOCKED. The whole split is the receiver --
 *  `PrintWriter`'s native constructor chains into real bytecode, so `lock`,
 *  `out`, `charOut` and `textOut` are populated whichever path built it;
 *  `PrintStream`'s writes `out` and `lock` and stops, and the VM-minted
 *  `System.out` is not constructed at all.
 *
 *  That record is a month old and four carrier fixes have landed since, so it
 *  is a HYPOTHESIS until this probe re-reads it. Section A reads the five
 *  fields the blocked list names, by reflection, on three differently-built
 *  receivers. A blocker that is already fixed is the single cheapest thing
 *  this lane can find (it has happened twice).
 *
 *  ## Why this probe writes to a FILE
 *
 *  Section 5 of the lane page: `System.out` is how every lane reads its
 *  probes, so a probe that reports only through `System.out` cannot report on
 *  `System.out`. Every row goes into a buffer that is written with a
 *  `FileOutputStream` -- no `PrintStream` in the path -- and is also echoed to
 *  `System.out` for a live run. THE FILE IS THE DIFF SOURCE. If the two
 *  disagree, that disagreement is itself the finding, and the last row says
 *  which rows reached the console.
 */
public class L4W6PrintCarrier {
    static final StringBuilder REPORT = new StringBuilder();
    static int rows = 0;

    static String esc(String s) {
        if (s == null) return "null";
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == '\n') b.append("\\n");
            else if (c == '\r') b.append("\\r");
            else if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }

    /** One row. Goes to the buffer first, so a console that discards cannot
     *  take the evidence with it. */
    static void p(String tag, Object v) {
        rows++;
        String line = esc(tag) + " |" + esc(String.valueOf(v)) + "|";
        REPORT.append(line).append('\n');
        System.out.println(line);
    }

    static void t(String tag, ThrowingRun r) {
        rows++;
        String line;
        try { r.run(); line = esc(tag) + " |no-throw|"; }
        catch (Throwable e) { line = esc(tag) + " |THREW " + e.getClass().getName() + "|"; }
        REPORT.append(line).append('\n');
        System.out.println(line);
    }

    interface ThrowingRun { void run() throws Throwable; }

    /** The SPECIES of a field's value, never its identity: a `BufferedWriter`
     *  is the answer, its address is not. `null` is the answer the blocked
     *  list predicts for four of the five. */
    static String species(Object o) {
        if (o == null) return "null";
        // `peek` reports an inaccessible field as a marker string, and a
        // marker whose SPECIES is `java.lang.String` reads exactly like a
        // field that really holds one. Pass it through instead.
        if (o instanceof String && ((String) o).startsWith("<UNREADABLE")) return (String) o;
        return o.getClass().getName();
    }

    static Object peek(Object target, Class<?> declaring, String field) {
        try {
            Field f = declaring.getDeclaredField(field);
            f.setAccessible(true);
            return f.get(target);
        } catch (Throwable e) {
            // An inaccessible field is a different answer from a null one and
            // must not be allowed to read as one.
            return "<UNREADABLE " + e.getClass().getSimpleName() + ">";
        }
    }

    // ------------------------------------------------------ A. the carrier

    /** The five fields the blocked list of W7-22 Section 3 names, plus the
     *  charset's CONCRETENESS -- an instance of the abstract
     *  `java.nio.charset.Charset` is the tell that it was fabricated. */
    static void carrierOf(String who, PrintStream s) {
        p(who + " class", s == null ? "null" : s.getClass().getName());
        if (s == null) return;
        p(who + ".out", species(peek(s, FilterOutputStream.class, "out")));
        p(who + ".closeLock", species(peek(s, FilterOutputStream.class, "closeLock")));
        p(who + ".charOut", species(peek(s, PrintStream.class, "charOut")));
        p(who + ".textOut", species(peek(s, PrintStream.class, "textOut")));
        Object cs = peek(s, PrintStream.class, "charset");
        p(who + ".charset", species(cs));
        // `Charset` is abstract, so a receiver whose charset IS a `Charset`
        // was allocated rather than resolved. Ask the question that way round
        // rather than by name, because the concrete name is platform-dependent
        // and the diff must not turn on the host's locale.
        p(who + ".charset is concrete",
          cs instanceof Charset && !cs.getClass().equals(Charset.class));
        p(who + ".charset abstract-exactly", cs != null && cs.getClass().equals(Charset.class));
    }

    static void carriers() throws Exception {
        carrierOf("System.out", System.out);
        carrierOf("System.err", System.err);
        // A user-constructed stream: the arm W7-22 measured at 26 of 26 ok.
        carrierOf("new PrintStream(BAOS,false,UTF_8)",
                  new PrintStream(new ByteArrayOutputStream(), false, StandardCharsets.UTF_8));
        // The one-argument ctor is the registered triple; this is the receiver
        // a NATIVE constructor builds, and the one whose `textOut` the record
        // says is null.
        carrierOf("new PrintStream(BAOS)", new PrintStream(new ByteArrayOutputStream()));
        carrierOf("new PrintStream(BAOS,true)", new PrintStream(new ByteArrayOutputStream(), true));
        // And the same question for the class whose verdict was RETIRABLE, so
        // the two halves are read off one instrument rather than two.
        PrintWriter pw1 = new PrintWriter(new ByteArrayOutputStream());
        pwCarrier("new PrintWriter(BAOS)", pw1);
        PrintWriter pw2 = new PrintWriter(new StringWriter());
        pwCarrier("new PrintWriter(StringWriter)", pw2);
        PrintWriter pw3 = new PrintWriter(System.out);
        pwCarrier("new PrintWriter(System.out)", pw3);
    }

    static void pwCarrier(String who, PrintWriter w) {
        p(who + " class", w.getClass().getName());
        p(who + ".out", species(peek(w, PrintWriter.class, "out")));
        p(who + ".lock", species(peek(w, Writer.class, "lock")));
        p(who + ".autoFlush", peek(w, PrintWriter.class, "autoFlush"));
    }

    // --------------------------------------------- B. the PrintWriter seven

    /** The seven `java/io/PrintWriter` triples the 2026-08-11 measurement
     *  called retirable, each asked for the BYTES it produced rather than for
     *  a return code -- section 4 of the lane page owns the silent wrong
     *  answer, and a `println` that discards returns void either way. */
    static void printWriterSeven(String arm, PrintWriter w, java.util.function.Supplier<String> read) {
        w.println("PW-S");                    // println(String)
        w.println();                          // println()
        w.println(7);                         // println(int)
        w.println((Object) Integer.valueOf(8)); // println(Object)
        w.write("PW-W");                      // write(String)
        w.write("PW-RANGE", 1, 3);            // write(String,int,int)
        w.flush();
        p(arm + " bytes", read.get());
    }

    static void printWriterArms() throws Exception {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        // <init>(OutputStream) is the seventh triple, and it is exercised by
        // being the thing that built this receiver.
        PrintWriter w = new PrintWriter(b);
        printWriterSeven("PW over BAOS", w, () -> new String(b.toByteArray(), StandardCharsets.UTF_8));

        StringWriter sw = new StringWriter();
        PrintWriter w2 = new PrintWriter(sw);
        printWriterSeven("PW over StringWriter", w2, sw::toString);

        // The arm that matters, and the JUnit ConsoleLauncher shape: a
        // PrintWriter whose sink is System.out. It chains real
        // BufferedWriter/OutputStreamWriter bytecode down onto a PrintStream
        // receiver, so it is the place the two verdicts meet.
        PrintWriter w3 = new PrintWriter(System.out);
        w3.println("PW-OVER-SYSOUT");
        w3.flush();
        p("PW over System.out", "written (see console-reached row)");

        // A PrintWriter built by the NATIVE constructor, with the methods
        // under test landing on it. The `<init>` yield is spent on a
        // throwaway so the receiver here is the native-built one.
        ByteArrayOutputStream b4 = new ByteArrayOutputStream();
        PrintWriter w4 = new PrintWriter(b4);
        w4.print("x");
        w4.flush();
        b4.reset();
        printWriterSeven("PW native-ctor receiver", w4,
                         () -> new String(b4.toByteArray(), StandardCharsets.UTF_8));

        // A PrintWriter over a Writer never enters a native at all; if this
        // row ever differs from the StringWriter row above, the claim that
        // `PrintWriter(Writer)` is unregistered has expired.
        StringWriter sw5 = new StringWriter();
        PrintWriter w5 = new PrintWriter(sw5, true);
        w5.println("auto");
        p("PW autoflush over Writer", sw5.toString());
    }

    // ------------------------------------------ C. what PrintStream discards

    /** Section 3's silence, asked directly. A retired `PrintStream` shadow
     *  over a fabricated `System.out` sets `trouble = true` and discards, so
     *  `checkError()` on `System.out` is the one row that distinguishes
     *  "printed" from "discarded" WITHOUT reading the console. */
    static void discardTells() {
        p("System.out.checkError", System.out.checkError());
        p("System.err.checkError", System.err.checkError());
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        PrintStream ps = new PrintStream(b, false, StandardCharsets.UTF_8);
        ps.println("PS-S");
        ps.print("PS-P");
        ps.printf("%s%d", "PS-F", 7);
        ps.write('!');
        ps.flush();
        p("user PrintStream bytes", new String(b.toByteArray(), StandardCharsets.UTF_8));
        p("user PrintStream checkError", ps.checkError());
        // close() is the row the record says NPEs on a native-built receiver.
        t("user PrintStream close", ps::close);
        ByteArrayOutputStream b2 = new ByteArrayOutputStream();
        PrintStream ps2 = new PrintStream(b2);
        ps2.print("y");
        t("one-arg-ctor PrintStream close", ps2::close);
        p("one-arg-ctor bytes", new String(b2.toByteArray(), StandardCharsets.UTF_8));
    }

    public static void main(String[] a) throws Exception {
        carriers();
        printWriterArms();
        discardTells();
        REPORT.append("rows ").append(rows).append('\n');
        REPORT.append("DONE L4W6PrintCarrier\n");
        System.out.println("rows " + rows);
        System.out.println("DONE L4W6PrintCarrier");
        // The report leaves through a FileOutputStream, which is not a
        // PrintStream and does not share its state. A run whose console is
        // empty still leaves this behind, and the difference between the two
        // is the measurement section 5 asks for.
        String out = a.length > 0 ? a[0] : "l4w6-carrier.txt";
        try (FileOutputStream f = new FileOutputStream(out)) {
            f.write(REPORT.toString().getBytes(StandardCharsets.UTF_8));
            f.flush();
        }
    }
}
