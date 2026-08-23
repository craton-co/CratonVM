import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;

/** What does an ofArray segment actually behave like on this VM? */
public class HeapSegmentShapeProbe {
    public static void main(String[] args) {
        int[] a = new int[8];
        MemorySegment s = MemorySegment.ofArray(a);
        System.out.printf("byteSize=%d%n", s.byteSize());
        s.set(ValueLayout.JAVA_INT, 4L, 0x11223344);
        System.out.printf("after set(4): a[1]=%08x read_back=%08x%n",
                a[1], s.get(ValueLayout.JAVA_INT, 4L));
        a[2] = 0x55667788;
        System.out.printf("after a[2]=..: seg.get(8)=%08x%n", s.get(ValueLayout.JAVA_INT, 8L));

        try (Arena arena = Arena.ofConfined()) {
            MemorySegment nat = arena.allocate(32, 8);
            for (int i = 0; i < 8; i++) {
                nat.set(ValueLayout.JAVA_INT, i * 4L, 0xA0 + i);
            }
            // heap -> native
            int[] src = new int[8];
            for (int i = 0; i < 8; i++) {
                src[i] = 0xB0 + i;
            }
            MemorySegment.copy(MemorySegment.ofArray(src), 0L, nat, 0L, 32L);
            System.out.printf("heap->native nat[0]=%08x nat[7]=%08x (want 000000b0 000000b7)%n",
                    nat.get(ValueLayout.JAVA_INT, 0L), nat.get(ValueLayout.JAVA_INT, 28L));
            // native -> heap
            int[] dst = new int[8];
            MemorySegment.copy(nat, 0L, MemorySegment.ofArray(dst), 0L, 32L);
            System.out.printf("native->heap dst[0]=%08x dst[7]=%08x (want 000000b0 000000b7)%n",
                    dst[0], dst[7]);
            // heap -> heap
            int[] h2 = new int[8];
            MemorySegment.copy(MemorySegment.ofArray(src), 0L, MemorySegment.ofArray(h2), 0L, 32L);
            System.out.printf("heap->heap   h2[0]=%08x h2[7]=%08x (want 000000b0 000000b7)%n",
                    h2[0], h2[7]);
        }
    }
}
