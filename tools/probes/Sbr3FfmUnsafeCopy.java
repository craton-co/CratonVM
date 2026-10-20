import java.lang.reflect.Field;
import java.util.Arrays;

/** run with --add-exports java.base/jdk.internal.misc=ALL-UNNAMED */
public class Sbr3FfmUnsafeCopy {
    public static void main(String[] a) throws Throwable {
        jdk.internal.misc.Unsafe u = jdk.internal.misc.Unsafe.getUnsafe();
        System.out.println("bases " + u.arrayBaseOffset(byte[].class) + " " + u.arrayBaseOffset(short[].class) + " " + u.arrayBaseOffset(char[].class)
            + " " + u.arrayBaseOffset(int[].class) + " " + u.arrayBaseOffset(long[].class) + " " + u.arrayBaseOffset(float[].class) + " " + u.arrayBaseOffset(double[].class));
        System.out.println("scales " + u.arrayIndexScale(byte[].class) + " " + u.arrayIndexScale(short[].class) + " " + u.arrayIndexScale(int[].class) + " " + u.arrayIndexScale(long[].class));
        long m = u.allocateMemory(32);
        for (int i = 0; i < 32; i++) u.putByte(m + i, (byte) (i + 1));
        int[] ia = new int[4];
        u.copyMemory(null, m, ia, u.arrayBaseOffset(int[].class), 16);
        System.out.println("copy->int[] " + Arrays.toString(ia));
        short[] sa = new short[8];
        u.copyMemory(null, m, sa, u.arrayBaseOffset(short[].class), 16);
        System.out.println("copy->short[] " + Arrays.toString(sa));
        byte[] ba = new byte[16];
        u.copyMemory(null, m, ba, u.arrayBaseOffset(byte[].class), 16);
        System.out.println("copy->byte[] " + ba[0] + " " + ba[15]);
        int[] ib = new int[4];
        u.copyMemory(ia, u.arrayBaseOffset(int[].class), ib, u.arrayBaseOffset(int[].class), 16);
        System.out.println("int[]->int[] " + Arrays.toString(ib));
        int[] ic = new int[4];
        u.setMemory(ic, u.arrayBaseOffset(int[].class), 16, (byte) 5);
        System.out.println("setMemory int[] " + Arrays.toString(ic));
        int[] id = new int[4];
        u.copyMemory(null, m, id, 16L, 16);
        System.out.println("copy->int[] at 16 " + Arrays.toString(id));
        u.freeMemory(m);
    }
}
