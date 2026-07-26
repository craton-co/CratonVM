import java.util.Arrays;

// HIB-CV-20 minimal repro: hot Arrays.fill([BIIB)V (triggers back-edge OSR at bci=10)
// while allocating to apply GC pressure. If the OSR-compiled fill mishandles the
// live byte[] root across the OSR-entry safepoint, the heap corrupts -> hang/CORRUPT.
public class OsrFillGc {
    static byte[] buf = new byte[300000];
    static Object[] junk = new Object[20000];

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        for (int r = 0; r < rounds; r++) {
            junk[r % junk.length] = new byte[512];          // GC pressure
            Arrays.fill(buf, 0, buf.length, (byte) r);       // OSR-hot fill
            if (buf[buf.length - 1] != (byte) r || buf[0] != (byte) r) {
                System.out.println("@@CORRUPT at r=" + r
                        + " buf[0]=" + buf[0] + " last=" + buf[buf.length - 1]);
                return;
            }
            if ((r & 0x3fff) == 0) { System.out.println("@@PROG r=" + r); System.out.flush(); }
        }
        System.out.println("@@DONE");
    }
}
