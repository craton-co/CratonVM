import java.lang.foreign.*;
import java.util.Arrays;

/** SBR3 ffmperf: native <-> heap-segment bulk copies (the RSegmentBulkCopy shapes) on the real classes. */
public class Sbr3FfmCopy {
    public static void main(String[] a) {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment src = arena.allocate(16, 8);
            for (int i = 0; i < 4; i++) src.set(ValueLayout.JAVA_INT, (long) i * 4, (i + 1) * 111);
            int[] dst = new int[4];
            MemorySegment.copy(src, 0L, MemorySegment.ofArray(dst), 0L, 16L);
            System.out.println("native->int[] " + Arrays.toString(dst));
            byte[] bd = new byte[16];
            MemorySegment.copy(src, 0L, MemorySegment.ofArray(bd), 0L, 16L);
            System.out.println("native->byte[] " + Arrays.toString(bd));
            long[] ld = new long[2];
            MemorySegment.copy(src, 0L, MemorySegment.ofArray(ld), 0L, 16L);
            System.out.println("native->long[] " + Arrays.toString(ld));
            short[] sd = new short[8];
            MemorySegment.copy(src, 0L, MemorySegment.ofArray(sd), 0L, 16L);
            System.out.println("native->short[] " + Arrays.toString(sd));
            int[] host = {7, 8, 9, 10};
            MemorySegment nd = arena.allocate(16, 8);
            MemorySegment.copy(MemorySegment.ofArray(host), 0L, nd, 0L, 16L);
            System.out.println("int[]->native " + nd.get(ValueLayout.JAVA_INT, 0) + " " + nd.get(ValueLayout.JAVA_INT, 4) + " " + nd.get(ValueLayout.JAVA_INT, 8) + " " + nd.get(ValueLayout.JAVA_INT, 12));
            int[] d2 = new int[4];
            MemorySegment.copy(src, ValueLayout.JAVA_INT, 0L, d2, 0, 4);
            System.out.println("native->int[] (typed) " + Arrays.toString(d2));
            int[] d3 = new int[4];
            MemorySegment.ofArray(d3).copyFrom(src);
            System.out.println("copyFrom " + Arrays.toString(d3));
            System.out.println("toArray " + Arrays.toString(src.toArray(ValueLayout.JAVA_INT)));
            int[] d4 = new int[8];
            MemorySegment.copy(src, 0L, MemorySegment.ofArray(d4), 8L, 16L);
            System.out.println("native->int[] off8 " + Arrays.toString(d4));
        }
    }
}
