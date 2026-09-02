import java.nio.charset.Charset;

/**
 * Every single-byte charset, encoded and decoded.
 *
 * All of them share `sun.nio.cs.SingleByte.initC2B`, which builds the encoder
 * table by allocating one 0x100 block per distinct high byte found in the
 * decode table, guarded by
 *
 *     if (c2bIndex[index] == UNMAPPABLE_ENCODING) { c2bIndex[index] = (char) off; off += 0x100; }
 *
 * A JIT bug made that gate true on EVERY iteration, so the loop allocated a
 * block per character and ran off the end of `c2b`. Sixteen of the twenty rows
 * below failed with `ExceptionInInitializerError` /
 * `ArrayIndexOutOfBoundsException` out of `<Charset>$Holder.<clinit>` — and on
 * a host whose default charset is one of them (a Russian-locale Windows box is
 * `windows-1251`), a bare `"...".getBytes()` with no explicit charset throws.
 *
 * Two rows are load-bearing controls and must NOT be dropped:
 *
 *   ISO-8859-1  the one table with no unmappable entry, so it needs exactly as
 *               many blocks as it allocates and passed THROUGHOUT. A sweep of
 *               only the broken charsets would have shown 100% failure and
 *               said nothing about which property mattered.
 *   UTF-8       not a SingleByte charset at all — the negative control for
 *               "did the whole charset subsystem break, or just this table?"
 *
 * Diff against HotSpot. Every row is a fixed hex string.
 */
public final class SingleByteCharsets {
    public static void main(String[] args) {
        String[] names = {
            "windows-1251", "windows-1252", "windows-1250", "windows-1253", "windows-1254",
            "windows-1255", "windows-1256", "windows-1257", "windows-1258",
            "ISO-8859-1", "ISO-8859-2", "ISO-8859-5", "ISO-8859-7", "ISO-8859-15",
            "KOI8-R", "KOI8-U", "IBM866", "x-MacCyrillic", "US-ASCII", "UTF-8",
        };
        for (String n : names) {
            System.out.println(n + " | encode=" + attempt(() -> {
                // Latin + Cyrillic, so the Cyrillic tables differ from each other.
                return hex("AaАа".getBytes(Charset.forName(n)));
            }) + " | decode=" + attempt(() -> {
                return hex(new String(new byte[] {(byte) 0x41, (byte) 0xC0, (byte) 0xE0},
                        Charset.forName(n)).toCharArray());
            }));
        }
    }

    interface Body { String run() throws Exception; }

    /** Print the failure instead of dying on it: a probe that stops early does
     *  not report less, it reports a shorter file that still diffs clean. */
    static String attempt(Body b) {
        try {
            return b.run();
        } catch (Throwable t) {
            StringBuilder sb = new StringBuilder("ERROR " + t.getClass().getSimpleName()
                    + ": " + t.getMessage());
            for (Throwable c = t.getCause(); c != null; c = c.getCause()) {
                sb.append(" <- ").append(c.getClass().getSimpleName()).append(": ").append(c.getMessage());
            }
            return sb.toString();
        }
    }

    static String hex(byte[] b) {
        StringBuilder s = new StringBuilder();
        for (byte x : b) s.append(String.format("%02x", x));
        return s.toString();
    }
    static String hex(char[] c) {
        StringBuilder s = new StringBuilder();
        for (char x : c) s.append(String.format("%04x", (int) x));
        return s.toString();
    }
}
