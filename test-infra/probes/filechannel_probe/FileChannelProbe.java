import java.io.File;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.StandardOpenOption;

// Regression probe for the arena-handle → FileDispatcher native-memory routing
// (continue_prompt_arena_native_memory_routing.md, R2). A FileChannel.write of a
// HEAP buffer forces the JDK through Util.getTemporaryDirectBuffer (an Unsafe
// arena handle) → FileDispatcherImpl.write0; until R2 lands this SIGSEGVs on the
// raw memcpy. Writes a HeapByteBuffer to a temp file and reads it back into a
// heap buffer, asserting the content round-trips. JDK-only, deterministic.
public class FileChannelProbe {
    public static void main(String[] args) throws Exception {
        File tmp = File.createTempFile("fcprobe", ".bin");
        tmp.deleteOnExit();
        byte[] payload = "filechannel-roundtrip-42".getBytes(StandardCharsets.UTF_8);

        try (FileChannel wc = FileChannel.open(tmp.toPath(),
                StandardOpenOption.WRITE, StandardOpenOption.TRUNCATE_EXISTING)) {
            ByteBuffer wb = ByteBuffer.wrap(payload); // heap buffer
            int written = 0;
            while (wb.hasRemaining()) {
                written += wc.write(wb);
            }
            System.out.println("written=" + written);
        }

        byte[] back = new byte[payload.length];
        try (FileChannel rc = FileChannel.open(tmp.toPath(), StandardOpenOption.READ)) {
            ByteBuffer rb = ByteBuffer.wrap(back); // heap buffer
            int total = 0;
            while (total < back.length) {
                int n = rc.read(rb);
                if (n < 0) break;
                total += n;
            }
            System.out.println("read=" + total);
        }

        String s = new String(back, StandardCharsets.UTF_8);
        System.out.println("content=" + s);
        System.out.println(s.equals(new String(payload, StandardCharsets.UTF_8)) ? "OK" : "MISMATCH");
        Files.deleteIfExists(tmp.toPath());
    }
}
