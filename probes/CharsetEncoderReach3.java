import java.io.*;
import java.util.logging.*;
import java.nio.charset.*;

/** StreamHandler(baos, fmt) works and ConsoleHandler() does not. The two differ
 *  in (a) the stream is System.err and (b) ConsoleHandler consults LogManager
 *  for its encoding. Separate them. */
public class CharsetEncoderReach3 {
    static int checks = 0, bad = 0;
    static void probe(String what, Runnable r) {
        checks++;
        try { r.run(); System.out.println("  ok   " + what); }
        catch (Throwable t) { bad++; System.out.println("  DIFF " + what + " -> "
                + t.getClass().getName() + ": " + t.getMessage()); }
    }
    public static void main(String[] a) {
        probe("System.err class", () ->
            System.out.println("        -> " + System.err.getClass().getName()));
        probe("StreamHandler(System.err, fmt)  [stream only]", () ->
            new StreamHandler(System.err, new SimpleFormatter()));
        probe("StreamHandler(baos, fmt).setEncoding(\"UTF-8\")", () -> {
            try { StreamHandler h = new StreamHandler(new ByteArrayOutputStream(), new SimpleFormatter());
                  h.setEncoding("UTF-8"); }
            catch (UnsupportedEncodingException e) { throw new RuntimeException(e); } });
        probe("LogManager.getLogManager()", () -> LogManager.getLogManager());
        probe("LogManager property .encoding", () -> System.out.println("        -> "
            + LogManager.getLogManager().getProperty("java.util.logging.ConsoleHandler.encoding")));
        probe("bare Handler.getEncoding()", () -> {
            Handler h = new StreamHandler(new ByteArrayOutputStream(), new SimpleFormatter());
            System.out.println("        -> encoding=" + h.getEncoding()); });
        probe("new ConsoleHandler()", () -> new ConsoleHandler());
        System.out.println(bad == 0 ? "PASS CharsetEncoderReach3 (" + checks + " checks)"
                                    : "FAIL CharsetEncoderReach3 (" + bad + " of " + checks + " wrong)");
    }
}
