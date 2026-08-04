import java.util.Arrays;

/**
 * Characterize the ArraysSupport.mismatch miscompile: which element type, which
 * lengths, and which mismatch position answer wrongly.
 *
 * ArraysSupport.mismatch only calls the vectorizedMismatch intrinsic when
 * length exceeds a per-type threshold (byte > 7, char/short > 3, int > 1,
 * long > 0); below it the method is a plain scalar loop. Scanning length from 1
 * upwards therefore separates "the scalar tail is miscompiled" from "the call
 * into the intrinsic is".
 */
public class MismatchShapeProbe {

    static int warm = 0;

    static void run(int len, int flip) {
        int[] ia = new int[len];
        int[] ib = new int[len];
        long[] la = new long[len];
        long[] lb = new long[len];
        byte[] ba = new byte[len];
        byte[] bb = new byte[len];
        char[] ca = new char[len];
        char[] cb = new char[len];
        for (int i = 0; i < len; i++) {
            ia[i] = i + 1;
            ib[i] = i + 1;
            la[i] = i + 1;
            lb[i] = i + 1;
            ba[i] = (byte) (i + 1);
            bb[i] = (byte) (i + 1);
            ca[i] = (char) (i + 1);
            cb[i] = (char) (i + 1);
        }
        ib[flip] = -99;
        lb[flip] = -99;
        bb[flip] = -99;
        cb[flip] = (char) 0xBEEF;

        int mi = Arrays.mismatch(ia, ib);
        int ml = Arrays.mismatch(la, lb);
        int mb = Arrays.mismatch(ba, bb);
        int mc = Arrays.mismatch(ca, cb);
        if (mi != flip || ml != flip || mb != flip || mc != flip) {
            System.out.printf("len=%-3d flip=%-3d int=%-4d long=%-4d byte=%-4d char=%-4d%n",
                    len, flip, mi, ml, mb, mc);
        }
    }

    public static void main(String[] args) {
        final int warmup = args.length > 0 ? Integer.parseInt(args[0]) : 60000;
        // Warm the method up on a shape that is answered correctly, so the
        // compile happens before the scan below.
        for (int i = 0; i < warmup; i++) {
            run(9, 4);
            warm++;
        }
        System.out.println("--- after " + warm + " warmup calls: divergent shapes ---");
        for (int len = 1; len <= 24; len++) {
            for (int flip = 0; flip < len; flip++) {
                run(len, flip);
            }
        }
        System.out.println("--- scan done ---");
    }
}
