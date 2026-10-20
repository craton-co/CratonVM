import java.lang.foreign.*;
import java.util.Arrays;

public class Sbr3FfmCopy2 {
    static String r(MemorySegment s, MemorySegment d, int n) { return d.get(ValueLayout.JAVA_BYTE, 0) + "," + d.get(ValueLayout.JAVA_BYTE, 4) + " base=" + d.address() + " ro=" + d.isReadOnly() + " nat=" + d.isNative(); }
    public static void main(String[] a) {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment src = arena.allocate(32, 8);
            for (int i = 0; i < 32; i++) src.set(ValueLayout.JAVA_BYTE, i, (byte) (i + 1));
            byte[] b = new byte[16]; MemorySegment hb = MemorySegment.ofArray(b); MemorySegment.copy(src, 0, hb, 0, 16); System.out.println("byte " + b[0] + " " + hb.byteSize() + " " + r(src, hb, 16));
            short[] s = new short[8]; MemorySegment hs = MemorySegment.ofArray(s); MemorySegment.copy(src, 0, hs, 0, 16); System.out.println("short " + s[0] + " " + hs.byteSize() + " " + r(src, hs, 16));
            char[] c = new char[8]; MemorySegment hc = MemorySegment.ofArray(c); MemorySegment.copy(src, 0, hc, 0, 16); System.out.println("char " + (int) c[0] + " " + hc.byteSize());
            int[] i4 = new int[4]; MemorySegment hi = MemorySegment.ofArray(i4); MemorySegment.copy(src, 0, hi, 0, 16); System.out.println("int " + i4[0] + " " + hi.byteSize() + " " + r(src, hi, 16));
            long[] l = new long[2]; MemorySegment hl = MemorySegment.ofArray(l); MemorySegment.copy(src, 0, hl, 0, 16); System.out.println("long " + l[0] + " " + hl.byteSize());
            float[] f = new float[4]; MemorySegment hf = MemorySegment.ofArray(f); MemorySegment.copy(src, 0, hf, 0, 16); System.out.println("float " + Float.floatToRawIntBits(f[0]) + " " + hf.byteSize());
            double[] d = new double[2]; MemorySegment hd = MemorySegment.ofArray(d); MemorySegment.copy(src, 0, hd, 0, 16); System.out.println("double " + Double.doubleToRawLongBits(d[0]) + " " + hd.byteSize());
            // heap -> heap, native -> native, and set through the heap segment
            int[] j = new int[4]; MemorySegment hj = MemorySegment.ofArray(j); hj.set(ValueLayout.JAVA_INT, 0, 77); System.out.println("int set " + j[0]);
            int[] k = new int[4]; MemorySegment.copy(hi, 0, MemorySegment.ofArray(k), 0, 16); System.out.println("int heap->heap " + k[0]);
            MemorySegment nd = arena.allocate(16, 8); MemorySegment.copy(src, 0, nd, 0, 16); System.out.println("nat->nat " + nd.get(ValueLayout.JAVA_BYTE, 0));
            int[] m = new int[4]; MemorySegment hm = MemorySegment.ofArray(m); hm.fill((byte) 5); System.out.println("fill " + m[0]);
        }
    }
}
