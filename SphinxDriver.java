import org.bouncycastle.pqc.crypto.test.Sphincs256Test;

// Drives Sphincs256Test.performTest() directly with explicit flush + Throwable
// capture, so the real failure (lost behind System.out buffering on the rc=1
// exit of the SimpleTest main) is visible.
public class SphinxDriver {
    public static void main(String[] args) throws Exception {
        try {
            System.out.println("[driver] start performTest"); System.out.flush();
            new Sphincs256Test().performTest();
            System.out.println("[driver] performTest returned OK"); System.out.flush();
        } catch (Throwable t) {
            System.out.println("[driver] THREW: " + t);
            t.printStackTrace(System.out);
            System.out.flush();
            System.err.flush();
            System.exit(2);
        }
        System.out.flush();
    }
}
