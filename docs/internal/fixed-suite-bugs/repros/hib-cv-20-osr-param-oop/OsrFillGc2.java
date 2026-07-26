import java.util.Arrays;

// HIB-CV-20 refined repro: the filled byte[] is freshly allocated each round
// (YOUNG, movable). If a young GC moves it during the OSR-entry rangeCheck
// safepoint call and the OSR-compiled fill keeps using the stale r15 array
// pointer, the fill writes out of bounds / to stale memory -> CORRUPT/crash/hang.
public class OsrFillGc2 {
    static Object[] junk = new Object[40000];

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 300000;
        for (int r = 0; r < rounds; r++) {
            junk[r % junk.length] = new int[64];            // churn young gen
            byte[] buf = new byte[20000];                   // FRESH young array
            Arrays.fill(buf, 0, buf.length, (byte) 0x5a);   // OSR-hot fill
            // validate every byte
            for (int k = 0; k < buf.length; k++) {
                if (buf[k] != 0x5a) {
                    System.out.println("@@CORRUPT r=" + r + " k=" + k + " v=" + buf[k]);
                    return;
                }
            }
            if ((r & 0x3fff) == 0) { System.out.println("@@PROG r=" + r); System.out.flush(); }
        }
        System.out.println("@@DONE");
    }
}
