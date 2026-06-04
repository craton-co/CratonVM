import java.nio.ByteBuffer;

// Regression probe for the non-gated DirectByteBuffer layout fix in
// `dbb_allocate_direct0` (native-io/src/direct_buffer.rs), which previously
// returned a buffer with capacity -1. Exercises allocateDirect + a put/flip/get
// round-trip and a slice/duplicate sanity check. JDK-only, deterministic.
public class DirectBufferProbe {
    public static void main(String[] args) {
        ByteBuffer b = ByteBuffer.allocateDirect(32);
        System.out.println("direct=" + b.isDirect());
        System.out.println("cap=" + b.capacity());
        System.out.println("pos0=" + b.position());
        b.put((byte) 10).put((byte) 20).put((byte) 30);
        System.out.println("posAfterPut=" + b.position());
        b.flip();
        System.out.println("limAfterFlip=" + b.limit());
        int g0 = b.get() & 0xff, g1 = b.get() & 0xff, g2 = b.get() & 0xff;
        System.out.println("get=" + g0 + "," + g1 + "," + g2);

        // duplicate shares content but has independent position/limit; at this
        // point b is position=3 limit=3, so rewind first for a stable view.
        b.rewind();
        ByteBuffer dup = b.duplicate();
        dup.position(1);
        ByteBuffer sl = dup.slice();
        System.out.println("sliceCap=" + sl.capacity() + " slice0=" + (sl.get(0) & 0xff));
        System.out.println("OK");
    }
}
