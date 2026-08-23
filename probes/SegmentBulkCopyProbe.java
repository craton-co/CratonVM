import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;

/**
 * Can a native MemorySegment be bulk-copied into a Java {@code int[]}
 * in one call?
 *
 * This is the whole model-load path for the CratonVM GPU inference
 * route: a GGUF f16 tensor is a native segment, and the kernel wants
 * its little-endian byte image as {@code int[]}. Per-element
 * {@code getFloat} would be 1.24 billion interpreter round trips for a
 * 1B-parameter model; one {@code MemorySegment.copy} per tensor is
 * 200-odd calls. The probe checks the copy is exact, in both
 * directions, and reports the rate.
 */
public class SegmentBulkCopyProbe {
    public static void main(String[] args) {
        int words = Integer.parseInt(System.getProperty("words", "4194304"));
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment src = arena.allocate((long) words * 4L, 8);
            for (int i = 0; i < words; i++) {
                src.set(ValueLayout.JAVA_INT, (long) i * 4L, i * 2654435761L != 0 ? i ^ 0x5bf03635 : i);
            }

            int[] dst = new int[words];
            String how = System.getProperty("how", "memcpy");
            long t0 = System.nanoTime();
            if (how.equals("memcpy")) {
                MemorySegment view = MemorySegment.ofArray(dst);
                MemorySegment.copy(src, 0L, view, 0L, (long) words * 4L);
            } else {
                MemorySegment.copy(src, ValueLayout.JAVA_INT, 0L, dst, 0, words);
            }
            long ns = System.nanoTime() - t0;

            int bad = 0;
            int firstBad = -1;
            for (int i = 0; i < words; i++) {
                int expect = src.get(ValueLayout.JAVA_INT, (long) i * 4L);
                if (dst[i] != expect) {
                    if (bad == 0) {
                        firstBad = i;
                    }
                    bad++;
                }
            }
            System.out.printf("COPY words=%d mismatches=%d first_bad=%d ns=%d MB_per_s=%.1f%n",
                    words, bad, firstBad, ns, (words * 4.0) / ns * 1000.0);
            System.out.printf("SPOT dst[0]=%08x dst[1]=%08x dst[last]=%08x%n",
                    dst[0], dst[1], dst[words - 1]);
        }
    }
}
