import java.nio.*;
import java.nio.charset.*;
import java.util.*;

/** L3 — `java.util.Base64`, 11 owning registrations with no coverage in this
 *  corpus.
 *
 *  Two records exist (`E14-1` on the fabricated receiver and its seven readers,
 *  `E5-1` on the null contract and a fourth site), both from lanes that could
 *  not run a binary. Neither left a probe behind, so whether the family is
 *  correct TODAY is unmeasured.
 *
 *  Base64 is a good differential: RFC 4648 fixes the output exactly, the three
 *  alphabets differ in two characters, and the padding, line-wrapping and
 *  strictness rules are the places an implementation quietly disagrees. Every
 *  row prints the ENCODED TEXT or the decoded bytes, never a length.
 *
 *  The MIME encoder is the interesting one: it wraps at 76 characters with CRLF
 *  and its decoder must SKIP characters outside the alphabet, where the basic
 *  decoder must reject them.
 */
public class Base64ShadowSweep {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
        }
    }

    static byte[] b(String s) { return s.getBytes(StandardCharsets.UTF_8); }

    static String s(byte[] a) { return a == null ? "null" : new String(a, StandardCharsets.UTF_8); }

    public static void main(String[] args) {
        Base64.Encoder E = Base64.getEncoder();
        Base64.Decoder D = Base64.getDecoder();
        Base64.Encoder U = Base64.getUrlEncoder();
        Base64.Decoder UD = Base64.getUrlDecoder();
        Base64.Encoder M = Base64.getMimeEncoder();
        Base64.Decoder MD = Base64.getMimeDecoder();

        // ---- the padding ladder: 0, 1 and 2 bytes of remainder
        tv("enc empty", () -> E.encodeToString(b("")));
        tv("enc 1 byte", () -> E.encodeToString(b("f")));
        tv("enc 2 bytes", () -> E.encodeToString(b("fo")));
        tv("enc 3 bytes", () -> E.encodeToString(b("foo")));
        tv("enc 4 bytes", () -> E.encodeToString(b("foob")));
        tv("enc 5 bytes", () -> E.encodeToString(b("fooba")));
        tv("enc 6 bytes", () -> E.encodeToString(b("foobar")));

        // ---- the two characters that separate the alphabets, forced out by
        // bytes that land on index 62 and 63.
        byte[] hi = {(byte) 0xfb, (byte) 0xff, (byte) 0xfe};
        tv("enc basic 62/63", () -> E.encodeToString(hi));
        tv("enc url 62/63", () -> U.encodeToString(hi));
        tv("enc url padding kept", () -> U.encodeToString(b("f")));
        tv("enc withoutPadding", () -> E.withoutPadding().encodeToString(b("f")));
        tv("enc url withoutPadding", () -> U.withoutPadding().encodeToString(b("fo")));

        // ---- decode, and the round trip
        tv("dec basic", () -> s(D.decode("Zm9vYmFy")));
        tv("dec padded 1", () -> s(D.decode("Zg==")));
        tv("dec padded 2", () -> s(D.decode("Zm8=")));
        tv("dec empty", () -> s(D.decode("")));
        tv("dec url 62/63", () -> Arrays.toString(UD.decode("-__-")));
        tv("round trip 255 bytes", () -> {
            byte[] all = new byte[256];
            for (int i = 0; i < 256; i++) all[i] = (byte) i;
            return Arrays.equals(all, D.decode(E.encodeToString(all)));
        });

        // ---- what the BASIC decoder must reject
        tv("dec unpadded 1", () -> s(D.decode("Zg")));
        tv("dec unpadded 2", () -> s(D.decode("Zm8")));
        tv("dec bad char", () -> s(D.decode("Zm9v!mFy")));
        tv("dec whitespace", () -> s(D.decode("Zm9v YmFy")));
        tv("dec newline", () -> s(D.decode("Zm9v\nYmFy")));
        tv("dec url char in basic", () -> s(D.decode("-__-")));
        tv("dec basic char in url", () -> Arrays.toString(UD.decode("+//+")));
        tv("dec single char", () -> s(D.decode("Z")));
        tv("dec padding only", () -> s(D.decode("====")));
        tv("dec trailing padding extra", () -> s(D.decode("Zg===")));
        tv("dec padding mid", () -> s(D.decode("Zg==Zg==")));
        tv("dec null", () -> s(D.decode((String) null)));
        tv("enc null", () -> E.encodeToString(null));

        // ---- MIME: wrapping at 76 with CRLF, and a lenient decoder
        tv("mime enc short", () -> M.encodeToString(b("foobar")));
        tv("mime enc wraps at 76", () -> {
            byte[] big = new byte[120];
            Arrays.fill(big, (byte) 'a');
            String out = M.encodeToString(big);
            int nl = out.indexOf("\r\n");
            return "firstLine=" + nl + " hasCRLF=" + out.contains("\r\n")
                    + " endsWithCRLF=" + out.endsWith("\r\n");
        });
        tv("mime dec skips junk", () -> s(MD.decode("Zm9v\r\nYmFy")));
        tv("mime dec skips illegal", () -> s(MD.decode("Zm9v!!!YmFy")));
        tv("mime dec unpadded", () -> s(MD.decode("Zg")));
        tv("mime custom width", () -> {
            byte[] big = new byte[40];
            Arrays.fill(big, (byte) 'a');
            String out = Base64.getMimeEncoder(8, new byte[] {'\n'}).encodeToString(big);
            return esc(out);
        });
        tv("mime width 0 = no wrap", () -> {
            byte[] big = new byte[80];
            Arrays.fill(big, (byte) 'a');
            String out = Base64.getMimeEncoder(0, new byte[] {'\n'}).encodeToString(big);
            return "hasNL=" + out.contains("\n") + " len=" + out.length();
        });
        tv("mime separator in alphabet", () -> Base64.getMimeEncoder(4, b("A")).getClass() != null);

        // ---- the array and ByteBuffer overloads, which are separate slots
        tv("encode(byte[]) returns bytes", () -> s(E.encode(b("foo"))));
        tv("encode into array exact", () -> {
            byte[] out = new byte[4];
            int n = E.encode(b("foo"), out);
            return n + " " + s(out);
        });
        tv("encode into array short", () -> {
            byte[] out = new byte[2];
            return E.encode(b("foo"), out);
        });
        tv("decode(byte[])", () -> s(D.decode(b("Zm9v"))));
        tv("decode into array", () -> {
            byte[] out = new byte[3];
            int n = D.decode(b("Zm9v"), out);
            return n + " " + s(out);
        });
        tv("encode ByteBuffer", () -> {
            ByteBuffer bb = E.encode(ByteBuffer.wrap(b("foo")));
            byte[] o = new byte[bb.remaining()];
            bb.get(o);
            return s(o);
        });
        tv("decode ByteBuffer", () -> {
            ByteBuffer bb = D.decode(ByteBuffer.wrap(b("Zm9v")));
            byte[] o = new byte[bb.remaining()];
            bb.get(o);
            return s(o);
        });

        // ---- identity: the encoders are cached singletons on HotSpot
        p("encoder same twice", Base64.getEncoder() == Base64.getEncoder());
        p("decoder same twice", Base64.getDecoder() == Base64.getDecoder());
        p("basic != url encoder", Base64.getEncoder() != Base64.getUrlEncoder());
        p("withoutPadding is new", Base64.getEncoder() != Base64.getEncoder().withoutPadding());
        p("encoder class", Base64.getEncoder().getClass().getName());
        p("decoder class", Base64.getDecoder().getClass().getName());

        System.out.println("DONE Base64ShadowSweep");
    }
}
