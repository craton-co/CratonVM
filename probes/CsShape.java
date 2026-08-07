import java.nio.charset.Charset;
import java.nio.charset.CharsetDecoder;

/**
 * What CLASS is the charset, and what class is its decoder?
 *
 * Three lines that settle an argument the `--dump-native-registry` census
 * cannot: it reports what is REGISTERED, and this reports what was
 * CONSTRUCTED. Under {@code --jdk-only} CratonVM answers
 * {@code java.nio.charset.Charset} and {@code java.nio.charset.CharsetDecoder}
 * where HotSpot answers {@code sun.nio.cs.US_ASCII} and
 * {@code sun.nio.cs.US_ASCII$Decoder} -- and both of CratonVM's answers name an
 * **abstract** class, which HotSpot can never instantiate.
 *
 * That is why sending {@code CharsetDecoder.decode} to real bytecode produces
 * {@code AbstractMethodError: ... decodeLoop ... has no Code attribute}: the
 * resolution is correct, the receiver is not. See
 * {@code jdk-only-step1-bytecode-available-RESOLVED-20260806.md}.
 *
 * Prints values rather than "ok", and a sentinel last, for the same reason
 * every probe in this directory does.
 */
public class CsShape {
    public static void main(String[] args) {
        try {
            Charset cs = Charset.forName("US-ASCII");
            System.out.println("charset.class=" + cs.getClass().getName());
            CharsetDecoder cd = cs.newDecoder();
            System.out.println("decoder.class=" + cd.getClass().getName());
            System.out.println("decoder.charset=" + cd.charset().getClass().getName());
            Charset u8 = Charset.forName("UTF-8");
            System.out.println("utf8.class=" + u8.getClass().getName());
            System.out.println("utf8.decoder=" + u8.newDecoder().getClass().getName());
            System.out.println("default=" + Charset.defaultCharset().getClass().getName());
            // An abstract receiver is the whole finding, so say it in one word
            // rather than leaving it to be read off six class names.
            System.out.println("abstractReceiver="
                    + java.lang.reflect.Modifier.isAbstract(cd.getClass().getModifiers()));
        } catch (Throwable t) {
            System.out.println("THREW " + t);
            t.printStackTrace(System.out);
        }
        System.out.println("CSSHAPE-DONE");
    }
}
