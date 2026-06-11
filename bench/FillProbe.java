// NETTY.1 retest probe: JIT'd java/util/Arrays.fill(byte[], byte) historically
// never returned (infinite loop after OSR at the 1000-backedge threshold,
// repro shape = StringUtil.<clinit> zero-filling a 65536-element HEX2B array
// with -1). Run the exact shape repeatedly and verify contents + termination.
import java.util.Arrays;

public final class FillProbe {
    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 50;
        long check = 0;
        for (int r = 0; r < reps; r++) {
            byte[] b = new byte[65536];
            Arrays.fill(b, (byte) -1);
            check += b[0] + b[65535] + b[r % 65536];

            int[] ints = new int[65536];
            Arrays.fill(ints, 0x5A5A5A5A);
            check += ints[0] + ints[65535];

            long[] longs = new long[32768];
            Arrays.fill(longs, 0x123456789ABCDEFL);
            check += longs[0] + longs[32767];

            char[] chars = new char[65536];
            Arrays.fill(chars, 'x');
            check += chars[0] + chars[65535];
        }
        System.out.println("done check=" + check);
    }
}
