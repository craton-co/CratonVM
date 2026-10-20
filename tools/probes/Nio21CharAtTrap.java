import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.charset.CharsetEncoder;
import java.nio.charset.StandardCharsets;

/**
 * Round nio21 wave 7, lane L2: the {@code CharSequence.charAt} trap of a class-predicated loop
 * ({@code CRATONVM_JIT_IR_SEQ_CHARAT_TRAP}) when its speculation keeps failing, and when the call
 * sits inside a {@code try} whose handler catches the out-of-range decline.
 *
 * <ul>
 *   <li>{@code flip}: {@code UTF_8$Encoder.encodeBufferLoop} over a {@code CharBuffer.wrap(String)}
 *       until it is compiled, then over {@code CharBuffer.wrap(StringBuilder)}: the loop's
 *       {@code charAt} now misses the String prefix on every char. The trap is charged at its site
 *       and, after {@code PER_BCI_DESPEC_LIMIT} charges, de-specialised there on EVERY sink
 *       ({@code r21-charat-trap-despec-skips-the-rerun-sinks-FIXED-20261012.md}), so the method stays
 *       compiled.</li>
 *   <li>{@code oob}: a user loop of {@code cs.charAt(i)} that runs one index past the end, inside
 *       {@code try { .. } catch (StringIndexOutOfBoundsException)}
 *       ({@code r21-ir-charat-trap-deopts-a-call-whose-exception-edges-are-armed-FIXED-20261012.md}).</li>
 * </ul>
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21CharAtTrap.java
 *   java     -cp /tmp/p21 Nio21CharAtTrap                       # HotSpot reference
 *   cratonvm -cp /tmp/p21 Nio21CharAtTrap                       # --jdk-only
 *   cratonvm --compatible -cp /tmp/p21 Nio21CharAtTrap
 *   # the re-run sinks: a per-bci de-spec line, no MakeNotCompilable for encodeBufferLoop
 *   CRATONVM_DBG_DEOPT=1 CRATONVM_JIT_DEOPT_SINK_RESUME=0 cratonvm -cp /tmp/p21 Nio21CharAtTrap \
 *       2>&amp;1 | grep -E 'per-bci de-spec|MakeNotCompilable' | grep -i encode
 *   CRATONVM_JIT_RERUN_SINK_PER_BCI_DESPEC=0 ...                 # the kill switch: the old storm
 *   # bench rows (ns/op):
 *   cratonvm -Dnio21.rows=bench -cp /tmp/p21 Nio21CharAtTrap
 * </pre>
 *
 * {@code -Dnio21.n=} sets the iteration count per phase (default 20 000).
 */
public class Nio21CharAtTrap {
    static final int N = Integer.getInteger("nio21.n", 20_000);
    static final String ROWS = System.getProperty("nio21.rows", "conf");

    static final String TEXT = "nio21 charAt trap: ascii only, long enough to loop over";

    static long encodeDigest(CharsetEncoder enc, CharSequence cs, ByteBuffer out) {
        out.clear();
        enc.reset();
        enc.encode(CharBuffer.wrap(cs), out, true);
        long h = out.position();
        for (int i = 0; i < out.position(); i++) {
            h = h * 31 + out.get(i);
        }
        return h;
    }

    static long flip(int n) {
        CharsetEncoder enc = StandardCharsets.UTF_8.newEncoder();
        ByteBuffer out = ByteBuffer.allocate(256);
        long d = 0;
        for (int i = 0; i < n; i++) {
            d += encodeDigest(enc, TEXT, out);
        }
        StringBuilder sb = new StringBuilder(TEXT);
        for (int i = 0; i < n; i++) {
            d += encodeDigest(enc, sb, out);
        }
        return d;
    }

    static int oobSum(CharSequence cs) {
        int s = 0;
        try {
            for (int i = 0; i <= cs.length(); i++) {
                s += cs.charAt(i);
            }
            return -1;
        } catch (StringIndexOutOfBoundsException e) {
            return s + e.getMessage().length();
        }
    }

    static long oob(int n) {
        long d = 0;
        for (int i = 0; i < n; i++) {
            d += oobSum(TEXT);
        }
        return d;
    }

    public static void main(String[] args) {
        System.out.println("conf flip " + flip(N));
        System.out.println("conf oob " + oob(N));
        String msg;
        try {
            TEXT.charAt(TEXT.length());
            msg = "none";
        } catch (StringIndexOutOfBoundsException e) {
            msg = e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println("conf oob-message " + msg);
        if (!ROWS.contains("bench")) {
            return;
        }
        for (int rep = 0; rep < 3; rep++) {
            long t0 = System.nanoTime();
            long d = flip(N);
            long t1 = System.nanoTime();
            System.out.println("flip " + (2L * N) + " " + ((t1 - t0) / (2L * N)) + " ns/op" + (d == 0 ? "!" : ""));
            t0 = System.nanoTime();
            d = oob(N);
            t1 = System.nanoTime();
            System.out.println("oob " + N + " " + ((t1 - t0) / N) + " ns/op" + (d == 0 ? "!" : ""));
        }
    }
}
