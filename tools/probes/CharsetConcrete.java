// Residual of `bug-printstream-charset-answers-the-abstract-base-20260825-FIXED-20260901.md`
// §5: "`charset_alloc`'s other callers are untouched."
//
// `java.nio.charset.Charset` is ABSTRACT. Any CratonVM path that fabricates an
// instance OF THE BASE hands back an object whose `newEncoder()` /
// `newDecoder()` / `contains()` have no Code attribute. This probe asks every
// door that can produce a Charset and, for each, (a) names the concrete class
// and (b) actually CONSUMES it, which is where the AbstractMethodError lands.
//
//   javac -d probes/out probes/CharsetConcrete.java
//   java -cp probes/out CharsetConcrete
import java.io.*;
import java.nio.charset.*;
import java.util.*;

public class CharsetConcrete {
    static int pass = 0, fail = 0;

    static void check(String label, Charset c) {
        if (c == null) { System.out.println("FAIL " + label + " -> null"); fail++; return; }
        String cls = c.getClass().getName();
        boolean abstractBase = cls.equals("java.nio.charset.Charset");
        String enc, dec, cont;
        try { enc = c.newEncoder() == null ? "null" : "ok"; }
        catch (Throwable t) { enc = t.getClass().getName(); }
        try { dec = c.newDecoder() == null ? "null" : "ok"; }
        catch (Throwable t) { dec = t.getClass().getName(); }
        try { cont = String.valueOf(c.contains(StandardCharsets.US_ASCII)); }
        catch (Throwable t) { cont = t.getClass().getName(); }
        boolean ok = !abstractBase && enc.equals("ok") && dec.equals("ok")
                     && (cont.equals("true") || cont.equals("false"));
        if (ok) pass++; else fail++;
        System.out.println((ok ? "PASS " : "FAIL ") + label
            + " -> " + c.name() + " [" + cls + "]"
            + " newEncoder=" + enc + " newDecoder=" + dec + " contains=" + cont);
    }

    public static void main(String[] a) throws Exception {
        check("StandardCharsets.UTF_8", StandardCharsets.UTF_8);
        check("StandardCharsets.US_ASCII", StandardCharsets.US_ASCII);
        check("StandardCharsets.ISO_8859_1", StandardCharsets.ISO_8859_1);
        check("StandardCharsets.UTF_16", StandardCharsets.UTF_16);
        check("StandardCharsets.UTF_16BE", StandardCharsets.UTF_16BE);
        check("StandardCharsets.UTF_16LE", StandardCharsets.UTF_16LE);
        check("Charset.defaultCharset()", Charset.defaultCharset());
        check("Charset.forName(UTF-8)", Charset.forName("UTF-8"));
        check("Charset.forName(utf8 alias)", Charset.forName("utf8"));
        check("Charset.forName(ISO-8859-1)", Charset.forName("ISO-8859-1"));
        check("Charset.forName(US-ASCII)", Charset.forName("US-ASCII"));
        check("System.out.charset()", System.out.charset());
        check("System.err.charset()", System.err.charset());
        check("new PrintStream(baos).charset()",
            new PrintStream(new ByteArrayOutputStream()).charset());
        check("new PrintStream(baos,true,\"UTF-8\").charset()",
            new PrintStream(new ByteArrayOutputStream(), true, "UTF-8").charset());
        // availableCharsets() values go through their own fabrication site.
        SortedMap<String, Charset> m = Charset.availableCharsets();
        System.out.println("availableCharsets size = " + m.size());
        for (String k : new String[] {"UTF-8", "US-ASCII", "ISO-8859-1"}) {
            Charset c = m.get(k);
            if (c == null) { System.out.println("FAIL availableCharsets[" + k + "] -> absent"); fail++; }
            else check("availableCharsets[" + k + "]", c);
        }
        // The consumer that actually crashed in the original report.
        try {
            new java.util.logging.ConsoleHandler();
            System.out.println("PASS new ConsoleHandler()"); pass++;
        } catch (Throwable t) {
            System.out.println("FAIL new ConsoleHandler() -> " + t); fail++;
        }
        try {
            new OutputStreamWriter(System.err).flush();
            System.out.println("PASS new OutputStreamWriter(System.err)"); pass++;
        } catch (Throwable t) {
            System.out.println("FAIL new OutputStreamWriter(System.err) -> " + t); fail++;
        }
        System.out.println((fail == 0 ? "PASS" : "FAIL") + " CharsetConcrete "
            + pass + "/" + (pass + fail));
    }
}
