import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

/**
 * Both open items in the loader/zip cluster have the SAME shape, which is why
 * this probe exists:
 *
 *   OriginTrackedYamlLoaderTests : StringBuilder grown over 233k appends -> toString()   (4 MiB)
 *   ZipContentTests.zip64Bytes() : ByteArrayOutputStream grown over 65537 writes
 *                                  -> toByteArray()                                     (~8 MB)
 *
 * Both then fail intermittently with SILENT CONTENT CORRUPTION - a truncated
 * line, or a zip whose central directory is not where its own header says.
 * That is a large, repeatedly-doubled array being copied; the suspect is the
 * copy/relocation of a big array rather than anything zip- or yaml-specific.
 *
 * Every variant verifies CONTENT, not just length: the failures are silent, so
 * a length check passes straight through them.
 *
 * Usage: GrowCopyStress <variant: baos|sb|raw|all> <reps> [garbagePerRep]
 */
public class GrowCopyStress {
    public static void main(String[] args) throws Exception {
        String variant = args.length > 0 ? args[0] : "all";
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int garbage = args.length > 2 ? Integer.parseInt(args[2]) : 200;

        for (int rep = 0; rep < reps; rep++) {
            if (variant.equals("baos") || variant.equals("all")) {
                String r = baos();
                if (r != null) { System.out.println("PROBE-FAIL rep=" + rep + " baos " + r); return; }
            }
            if (variant.equals("sb") || variant.equals("all")) {
                String r = sb();
                if (r != null) { System.out.println("PROBE-FAIL rep=" + rep + " sb " + r); return; }
            }
            if (variant.equals("raw") || variant.equals("all")) {
                String r = raw();
                if (r != null) { System.out.println("PROBE-FAIL rep=" + rep + " raw " + r); return; }
            }
            churn(garbage);
            System.out.println("rep " + rep + " ok");
        }
        System.out.println("PROBE-OK " + reps + " reps variant=" + variant);
    }

    /** zip64Bytes()'s shape: many small writes into a doubling ByteArrayOutputStream. */
    static String baos() {
        final int n = 65537;
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (int i = 0; i < n; i++) {
            out.write(("Entry " + (i + 1) + "\n").getBytes(StandardCharsets.UTF_8), 0,
                    ("Entry " + (i + 1) + "\n").length());
        }
        byte[] b = out.toByteArray();
        // Rebuild the expectation and compare byte for byte.
        StringBuilder want = new StringBuilder();
        for (int i = 0; i < n; i++) want.append("Entry ").append(i + 1).append('\n');
        byte[] w = want.toString().getBytes(StandardCharsets.UTF_8);
        if (b.length != w.length) return "length got=" + b.length + " want=" + w.length;
        int bad = Arrays.mismatch(b, w);
        return bad < 0 ? null : "firstBad=" + bad + " got=" + (b[bad] & 0xff) + " want=" + (w[bad] & 0xff);
    }

    /** The yaml test's shape. */
    static String sb() {
        final String LINE = "- some list entry\n";
        StringBuilder yaml = new StringBuilder();
        while (yaml.length() < 4_194_304) yaml.append(LINE);
        String s = yaml.toString();
        int n = LINE.length();
        for (int i = 0; i < s.length(); i++) {
            if (s.charAt(i) != LINE.charAt(i % n))
                return "firstBad=" + i + " line=" + (i / n) + " got=" + (int) s.charAt(i);
        }
        byte[] b = s.getBytes(StandardCharsets.UTF_8);
        if (b.length != s.length()) return "utf8 length " + b.length + " vs " + s.length();
        return null;
    }

    /** The mechanism alone: manual doubling via Arrays.copyOf, no library in the way. */
    static String raw() {
        byte[] buf = new byte[64];
        int len = 0;
        final int target = 8 << 20;
        while (len < target) {
            if (len == buf.length) buf = Arrays.copyOf(buf, buf.length * 2);
            buf[len] = (byte) (len % 251);
            len++;
        }
        byte[] fin = Arrays.copyOf(buf, len);
        for (int i = 0; i < len; i++) {
            if (fin[i] != (byte) (i % 251))
                return "firstBad=" + i + " got=" + fin[i] + " want=" + (byte) (i % 251);
        }
        return null;
    }

    /** Allocation between reps so a collection actually happens mid-growth. */
    static void churn(int mb) {
        Object[] keep = new Object[8];
        for (int i = 0; i < mb; i++) keep[i % keep.length] = new byte[1 << 20];
        if (keep[0] == null) System.out.println("unreachable");
    }
}
