import java.nio.charset.Charset;

import org.apache.tomcat.util.buf.CharsetCache;

/**
 * Exercises the exact ConcurrentHashMap read path used by Tomcat's lazy
 * CharsetCache entries after they have been resolved once.
 */
public final class CharsetCacheLazyArmProbe {
    private static final String[] NAMES = {
        "ISO-8859-1", "ISO-8859-2", "ISO-8859-3", "ISO-8859-4", "ISO-8859-5"
    };
    private static volatile long sink;

    public static void main(String[] args) {
        int iterations = args.length == 0 ? 1_000 : Integer.parseInt(args[0]);
        CharsetCache cache = new CharsetCache();

        for (String name : NAMES) {
            if (cache.getCharset(name) == null) {
                throw new AssertionError("missing charset: " + name);
            }
        }

        long local = 0;
        for (int i = 0; i < iterations; i++) {
            Charset charset = cache.getCharset(NAMES[i % NAMES.length]);
            if (charset == null) {
                throw new AssertionError("lost cached charset");
            }
            local += charset.hashCode();
        }
        sink = local;
        System.out.println("CHARSET_LAZY_ARM_OK sink=" + sink);
    }
}
