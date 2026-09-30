import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.util.ArrayList;
import java.util.List;

/**
 * `-XX:MaxMetaspaceSize` is enforced (gc-common round, wave 6, lane F6;
 * `docs/internal/gc-common-round-20260923/common-w5f-max-metaspace-size-is-not-enforced-FIXED-20260923.md`).
 *
 * Run with `-XX:MaxMetaspaceSize=32m`:
 *
 *   keep  (default) — defines one ~4.5 KB class in each of up to 200000 fresh
 *                     loaders and KEEPS every loader: must end in
 *                     `OutOfMemoryError: Metaspace`, as on HotSpot.
 *   drop            — the same defines, each loader dropped at once: must run
 *                     to completion (a define over the limit collects first,
 *                     and class unloading gives the dead loaders' charge back).
 *
 * Prints `PROBE-OK` / `PROBE-FAIL`. The define count at the OOME differs from
 * HotSpot's (CratonVM charges class-file bytes, HotSpot its own metadata) and
 * is printed on a separate `info` line.
 */
public class MetaspaceLimitProbe {
    static final int DEFINES = 200_000;
    static final String NAME = "MetaspaceLimitProbe$Payload";

    static final String K =
        "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyz!?";

    /** One ~4 KB constant (javac folds the concatenation into a single Utf8 entry). */
    public static final class Payload {
        public static final String BLOB = K + K + K + K + K + K + K + K + K + K
            + K + K + K + K + K + K + K + K + K + K
            + K + K + K + K + K + K + K + K + K + K
            + K + K + K + K + K + K + K + K + K + K;
        public int touch() { return BLOB.length(); }
    }

    static final class OneShot extends ClassLoader {
        OneShot() { super(null); }
        Class<?> def(byte[] b) { return defineClass(NAME, b, 0, b.length); }
    }

    static byte[] payloadBytes() throws Exception {
        try (InputStream in = MetaspaceLimitProbe.class.getResourceAsStream(NAME + ".class")) {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[8192];
            for (int n; (n = in.read(buf)) > 0; ) out.write(buf, 0, n);
            return out.toByteArray();
        }
    }

    public static void main(String[] args) throws Exception {
        boolean keep = args.length == 0 || !args[0].equals("drop");
        byte[] bytes = payloadBytes();
        List<Object> kept = new ArrayList<>();
        int defined = 0;
        try {
            for (; defined < DEFINES; defined++) {
                OneShot loader = new OneShot();
                Class<?> c = loader.def(bytes);
                if (keep) {
                    kept.add(loader);
                    kept.add(c);
                }
            }
        } catch (OutOfMemoryError e) {
            kept.clear();
            System.out.println("info: defines=" + defined + " classBytes=" + bytes.length);
            boolean metaspace = "Metaspace".equals(e.getMessage());
            System.out.println((keep ? "keep" : "drop") + ": OutOfMemoryError: " + e.getMessage()
                + (keep && metaspace ? " PROBE-OK" : " PROBE-FAIL"));
            return;
        } catch (Throwable t) {
            kept.clear();
            System.out.println((keep ? "keep" : "drop") + ": " + t + " after " + defined + " PROBE-FAIL");
            return;
        }
        kept.clear();
        System.out.println("info: defines=" + defined + " classBytes=" + bytes.length);
        System.out.println((keep ? "keep" : "drop") + ": completed"
            + (keep ? " without OutOfMemoryError PROBE-FAIL" : " PROBE-OK"));
    }
}
