import java.io.File;
import java.io.RandomAccessFile;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Field;
import java.nio.ByteBuffer;
import java.nio.MappedByteBuffer;
import java.nio.channels.FileChannel;

/** L1 R5: does the null-base fallback come from the METHODHANDLE, or from
 *  `invokeCleaner` itself?
 *
 *  The fallback is now attributed to a frame, not just a class:
 *
 *    Unsafe <- MappedByteBufferIndexInputProvider.lambda$newBufferCleaner$0+19
 *           <- ByteBufferGuard.invalidateAndUnmap+48
 *           <- ByteBufferIndexInput.close+38
 *
 *  bci 19 of that lambda is `unmapper.invokeExact(buffer)`, and `unmapper` is
 *  a MethodHandle bound to `Unsafe.invokeCleaner`. `nargs` has already refuted
 *  the "arguments were truncated" reading: the calls arrive with full arity
 *  (3 for getIntVolatile, 5 for compareAndSwapInt), so the null base and the
 *  0 offset are both genuine.
 *
 *  This reproduces Lucene 9.7's unmap hack in 40 lines and pairs it with the
 *  CONTROL that isolates the MethodHandle: the SAME call made directly.
 *
 *    * warn on BOTH arms  -> `invokeCleaner`'s own path is the producer, and
 *                            the MethodHandle is incidental.
 *    * warn on the MH arm ONLY -> MethodHandle dispatch is the producer.
 *    * warn on NEITHER    -> this is not the shape; the reproduction is
 *                            missing something Lucene does (and THAT is the
 *                            result, not a silence to read as absence).
 *
 *  The measurement is on STDERR (`UNCLASSIFIED-NULL-BASE`). stdout only says
 *  which arm ran, so the arms can be told apart in the log.
 */
public class UnmapHackProbe {

    static MappedByteBuffer map(File f) throws Exception {
        try (RandomAccessFile raf = new RandomAccessFile(f, "rw")) {
            raf.setLength(65536);
            try (FileChannel ch = raf.getChannel()) {
                return ch.map(FileChannel.MapMode.READ_WRITE, 0, 65536);
            }
        }
    }

    public static void main(String[] args) throws Throwable {
        Class<?> unsafeClass = Class.forName("sun.misc.Unsafe");
        Field th = unsafeClass.getDeclaredField("theUnsafe");
        th.setAccessible(true);
        Object theUnsafe = th.get(null);

        File tmp = File.createTempFile("l1r5unmap", ".dat");
        tmp.deleteOnExit();

        // ---- ARM 1: through a MethodHandle, exactly as Lucene does --------
        System.out.println("ARM-MH begin");
        MethodHandle unmapper = MethodHandles.lookup()
                .findVirtual(unsafeClass, "invokeCleaner",
                             MethodType.methodType(void.class, ByteBuffer.class))
                .bindTo(theUnsafe);
        MappedByteBuffer b1 = map(tmp);
        b1.putInt(0, 0x11111111);
        unmapper.invoke((ByteBuffer) b1);
        System.out.println("ARM-MH done");

        // ---- ARM 2: the CONTROL -- the same call, made directly ----------
        // Reflection, not a direct call, only because `sun.misc.Unsafe` is not
        // on the compile classpath. Reflection is NOT MethodHandle dispatch,
        // which is the axis under test.
        System.out.println("ARM-DIRECT begin");
        MappedByteBuffer b2 = map(tmp);
        b2.putInt(0, 0x22222222);
        unsafeClass.getMethod("invokeCleaner", ByteBuffer.class)
                   .invoke(theUnsafe, (ByteBuffer) b2);
        System.out.println("ARM-DIRECT done");

        System.out.println("DONE UnmapHackProbe");
    }
}
