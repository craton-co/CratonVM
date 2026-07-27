import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.channels.ClosedByInterruptException;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/**
 * Regression: BUG-NIO-NULL-INTERRUPTOR-20260726.
 *
 * `AbstractInterruptibleChannel.interruptor` is a FINAL field the JDK
 * constructor always assigns; `begin()` dereferences it unconditionally once
 * `Thread.currentThread().isInterrupted()` is true. CratonVM builds
 * `FileChannelImpl` through a native bridge that never runs that constructor and
 * used to leave the slot null, so ANY channel operation on a thread whose
 * interrupt flag happened to be set died with
 * `NullPointerException: Cannot invoke "sun.nio.ch.Interruptible.interrupt(...)"
 * because "this.interruptor" is null` instead of performing the specified
 * asynchronous close. (Found via H2 `TestStreamStore`, where the interrupt
 * itself was a second bug — see `RExecutorShutdown`.)
 *
 * The specified behaviour, asserted here against both VMs: the operation fails
 * with `ClosedByInterruptException`, the channel ends up closed, and the
 * thread's interrupt status is preserved.
 */
public class RChannelInterrupt {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    public static void main(String[] args) throws Exception {
        Path p = Files.createTempFile("rchanint", ".dat");
        try {
            // Baseline: a normal, non-interrupted write works.
            try (FileChannel fc = FileChannel.open(p, StandardOpenOption.WRITE)) {
                check(fc.write(ByteBuffer.wrap(new byte[64]), 0) == 64, "plain write");
                check(fc.isOpen(), "channel should still be open");
            }

            // Interrupted thread: the write must raise ClosedByInterruptException
            // (asynchronous close), not NullPointerException.
            FileChannel fc = FileChannel.open(p, StandardOpenOption.WRITE);
            String outcome;
            boolean stillOpen;
            Thread.currentThread().interrupt();
            try {
                fc.write(ByteBuffer.wrap(new byte[64]), 0);
                outcome = "no-exception";
            } catch (ClosedByInterruptException e) {
                outcome = "ClosedByInterruptException";
            } catch (IOException e) {
                outcome = "IOException:" + e.getClass().getName();
            } catch (RuntimeException e) {
                outcome = "RuntimeException:" + e.getClass().getName();
            }
            // Interrupt status survives the operation; clear it so the rest of
            // this test (and the VM's own shutdown) is not affected.
            boolean flagKept = Thread.interrupted();
            stillOpen = fc.isOpen();
            try {
                fc.close();
            } catch (IOException ignored) {
                // already closed by the interrupt machinery
            }

            check(outcome.equals("ClosedByInterruptException"),
                    "interrupted write outcome was " + outcome
                            + " (a NullPointerException here means interruptor is null)");
            check(flagKept, "interrupt status must survive ClosedByInterruptException");
            check(!stillOpen, "the channel must be closed after an interrupted operation");

            System.out.println("CK RChannelInterrupt checks=" + checks);
            System.out.println("PASS RChannelInterrupt");
        } finally {
            Files.deleteIfExists(p);
        }
    }
}
