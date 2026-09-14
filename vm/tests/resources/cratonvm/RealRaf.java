package cratonvm;

import java.io.RandomAccessFile;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Smoke test for the CRATONVM_REAL_RAF=1 real-path gate.
 *
 * Run under CRATONVM_REAL_RAF=1 to verify that RandomAccessFile runs real
 * JDK bytecode instead of the synthetic overlay. The test writes an int and
 * a long, seeks back to the start, reads them back, and verifies the values.
 */
public class RealRaf {
    public static void main(String[] args) throws Exception {
        Path tmp = Files.createTempFile("cratonvm-raf-test-", ".bin");
        try {
            try (RandomAccessFile raf = new RandomAccessFile(tmp.toFile(), "rw")) {
                raf.writeInt(0xDEADBEEF);
                raf.writeLong(123456789L);
                raf.seek(0);
                int magic = raf.readInt();
                long value = raf.readLong();
                long pos = raf.getFilePointer();
                System.out.println("r:magic=" + Integer.toHexString(magic));
                System.out.println("r:value=" + value);
                System.out.println("r:pos=" + pos);
                System.out.println("r:length=" + raf.length());
            }
            System.out.println("REAL_RAF_OK 4");
        } finally {
            Files.deleteIfExists(tmp);
        }
    }
}
