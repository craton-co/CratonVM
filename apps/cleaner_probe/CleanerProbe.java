import java.lang.ref.Cleaner;
import java.nio.ByteBuffer;
import java.util.concurrent.atomic.AtomicBoolean;

public class CleanerProbe {
    public static void main(String[] args) throws Exception {
        Cleaner cleaner = Cleaner.create();
        AtomicBoolean ran = new AtomicBoolean(false);

        // Allocate direct ByteBuffer
        ByteBuffer buf = ByteBuffer.allocateDirect(1024);
        buf.putInt(0, 42);
        System.out.println("buf.cap=" + buf.capacity() + " val=" + buf.getInt(0));

        // Register cleanup action with the cleaner against an Object holder
        Object holder = new Object();
        Cleaner.Cleanable cleanable = cleaner.register(holder, () -> {
            ran.set(true);
            System.out.println("cleaner.action.invoked");
        });

        // Drop strong reference and request GC
        holder = null;
        for (int i = 0; i < 5 && !ran.get(); i++) {
            System.gc();
            Thread.sleep(50);
        }

        // Force-clean (idempotent) for assertion
        cleanable.clean();
        System.out.println("ran=" + ran.get());
        if (!ran.get()) throw new AssertionError("cleaner did not run");
        System.out.println("CleanerProbe: PASS");
    }
}
