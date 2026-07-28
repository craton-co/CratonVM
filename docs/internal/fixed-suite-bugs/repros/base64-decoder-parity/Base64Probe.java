import java.util.Base64;

/** Prints one deterministic line per java.util.Base64 edge case so CratonVM
 *  output can be diffed byte-for-byte against real HotSpot. */
public class Base64Probe {

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) sb.append(String.format("%02x", x));
        return sb.toString();
    }

    interface Dec { byte[] run() throws Exception; }

    static void t(String label, Dec d) {
        String out;
        try {
            out = "OK[" + hex(d.run()) + "]";
        } catch (Throwable e) {
            out = e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println(label + " => " + out);
    }

    public static void main(String[] args) {
        Base64.Decoder basic = Base64.getDecoder();
        Base64.Decoder url = Base64.getUrlDecoder();
        Base64.Decoder mime = Base64.getMimeDecoder();

        // --- string inputs, basic decoder ---
        String[] basicStrings = {
            "", "A", "AB", "AB=", "AB==", "ABC", "ABC=", "ABCD", "ABCDE", "ABCDEF",
            "=", "=AAA", "A=AA", "AB=C", "AB==CD", "ABCD=", "ABCD==",
            "not valid base64", "not base 64", "-_", "a+b/", "AB\r\nCD", "AB CD",
        };
        for (String s : basicStrings) {
            t("basic(\"" + s.replace("\r", "\\r").replace("\n", "\\n") + "\")", () -> basic.decode(s));
        }

        // --- same inputs, url + mime decoders ---
        for (String s : basicStrings) {
            t("url(\"" + s.replace("\r", "\\r").replace("\n", "\\n") + "\")", () -> url.decode(s));
        }
        for (String s : basicStrings) {
            t("mime(\"" + s.replace("\r", "\\r").replace("\n", "\\n") + "\")", () -> mime.decode(s));
        }

        // --- raw byte inputs exercising the signed-hex message ---
        byte[][] rawes = {
            new byte[] { 'A', 'B', (byte) 0x80, 'D' },
            new byte[] { 'A', 'B', (byte) 0xff, 'D' },
            new byte[] { 'A', 'B', (byte) 0xc3, 'D' },
            new byte[] { 'A', 'B', 0x00, 'D' },
            new byte[] { 'A', 'B', 0x09, 'D' },
        };
        for (byte[] r : rawes) {
            t("basicBytes(" + hex(r) + ")", () -> basic.decode(r));
            t("mimeBytes(" + hex(r) + ")", () -> mime.decode(r));
        }

        // --- non-latin1 string input (decode(String) is getBytes(ISO_8859_1)) ---
        t("basic(\"AB\\u00ffD\")", () -> basic.decode("AB\u00ffD"));
        t("basic(\"AB\\u20acD\")", () -> basic.decode("AB\u20acD"));

        // --- encoder: MIME line separators, no trailing CRLF ---
        for (int n : new int[] { 0, 1, 2, 3, 56, 57, 58, 114, 115 }) {
            byte[] data = new byte[n];
            for (int i = 0; i < n; i++) data[i] = (byte) i;
            String enc = Base64.getMimeEncoder().encodeToString(data);
            System.out.println("mimeEnc(" + n + ") => len=" + enc.length()
                + " [" + enc.replace("\r", "\\r").replace("\n", "\\n") + "]");
        }
        System.out.println("basicEnc(255,255) => " + Base64.getEncoder().encodeToString(new byte[] { -1, -1 }));
        System.out.println("urlEncNoPad(255,255) => "
            + Base64.getUrlEncoder().withoutPadding().encodeToString(new byte[] { -1, -1 }));

        // --- round trip ---
        byte[] round = new byte[300];
        for (int i = 0; i < round.length; i++) round[i] = (byte) (i * 7);
        String encoded = Base64.getMimeEncoder().encodeToString(round);
        System.out.println("mimeRoundTrip => " + hex(Base64.getMimeDecoder().decode(encoded)).equals(hex(round)));
        String benc = Base64.getEncoder().encodeToString(round);
        System.out.println("basicRoundTrip => " + hex(Base64.getDecoder().decode(benc)).equals(hex(round)));
    }
}
