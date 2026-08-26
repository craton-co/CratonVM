import java.nio.charset.*;
import java.io.*;

/** How much of java.io is blocked by Charset.newEncoder/newDecoder in --jdk-only?
 *  Each check is independent so one failure does not hide the rest. */
public class CharsetEncoderReach {
    static int checks = 0, bad = 0;
    static void probe(String what, Runnable r) {
        checks++;
        try { r.run(); System.out.println("  ok   " + what); }
        catch (Throwable t) { bad++; System.out.println("  DIFF " + what + " -> " + t.getClass().getName()
                                                        + ": " + t.getMessage()); }
    }
    public static void main(String[] a) {
        probe("Charset.forName(UTF-8)", () -> Charset.forName("UTF-8"));
        probe("Charset.defaultCharset()", () -> Charset.defaultCharset());
        probe("Charset.forName(UTF-8).newEncoder()", () -> Charset.forName("UTF-8").newEncoder());
        probe("Charset.forName(UTF-8).newDecoder()", () -> Charset.forName("UTF-8").newDecoder());
        probe("Charset.defaultCharset().newEncoder()", () -> Charset.defaultCharset().newEncoder());
        probe("Charset.forName(ISO-8859-1).newEncoder()", () -> Charset.forName("ISO-8859-1").newEncoder());
        probe("Charset.forName(US-ASCII).newEncoder()", () -> Charset.forName("US-ASCII").newEncoder());
        probe("new OutputStreamWriter(baos)", () -> {
            new OutputStreamWriter(new ByteArrayOutputStream()); });
        probe("new OutputStreamWriter(baos, UTF-8)", () -> {
            new OutputStreamWriter(new ByteArrayOutputStream(), java.nio.charset.StandardCharsets.UTF_8); });
        probe("new InputStreamReader(bais)", () -> {
            new InputStreamReader(new ByteArrayInputStream(new byte[]{65})); });
        probe("new PrintWriter(baos)", () -> new PrintWriter(new ByteArrayOutputStream()));
        probe("new PrintStream(baos, true, UTF-8)", () -> {
            try { new PrintStream(new ByteArrayOutputStream(), true, "UTF-8"); }
            catch (UnsupportedEncodingException e) { throw new RuntimeException(e); } });
        probe("String.getBytes(UTF-8)", () -> "hi".getBytes(java.nio.charset.StandardCharsets.UTF_8));
        probe("encode a string through the encoder", () -> {
            try { Charset.forName("UTF-8").newEncoder()
                    .encode(java.nio.CharBuffer.wrap("hello")); }
            catch (CharacterCodingException e) { throw new RuntimeException(e); } });
        System.out.println(bad == 0 ? "PASS CharsetEncoderReach (" + checks + " checks)"
                                    : "FAIL CharsetEncoderReach (" + bad + " of " + checks + " wrong)");
    }
}
