import java.io.RandomAccessFile;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/**
 * W7-72-ssc-socket-and-filechannel.md item 2: is {@code FileChannel.isOpen()}
 * right in BOTH directions?
 *
 * <p>The defect: CratonVM's synthetic {@code java.nio.channels.FileChannel} kept
 * its file position in object slot 1, which on the real class is
 * {@code AbstractInterruptibleChannel.closed} — the {@code volatile boolean}
 * whose negation {@code isOpen()} returns. So a channel whose position moved off
 * zero could report itself CLOSED to real JDK bytecode.
 *
 * <p><b>The over-correction guard is the whole point of this probe.</b> A repair
 * that made {@code isOpen()} uniformly {@code false} passes any test that only
 * checks "a closed channel reports closed". Section 1 therefore asserts the
 * OPEN direction first and at every step, and section 3 asserts it after a
 * position move, a read, a write and a {@code force} — the operations that used
 * to poison the field. Section 2 is the closed direction. A VM that answers
 * {@code true} everywhere and a VM that answers {@code false} everywhere both
 * fail this probe; only one that tracks the transition passes.
 *
 * <p>Every {@code isOpen()} here is reached the way an application reaches it,
 * through the public API, not through the native that wrote the slot.
 *
 * <p>Run on HotSpot 25.0.3+9 for the oracle; transcript at the bottom.
 */
public class FileChannelIsOpenProbe {

    static String show(ThrowingSupplier<?> s) {
        try {
            return String.valueOf(s.get());
        } catch (Throwable t) {
            return t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
    }

    interface ThrowingSupplier<T> {
        T get() throws Throwable;
    }

    public static void main(String[] args) throws Exception {
        Path tmp = Files.createTempFile("fc-isopen-probe", ".bin");
        try {
            Files.write(tmp, new byte[] {1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12});
            openDirection(tmp);
            closedDirection(tmp);
            positionMoves(tmp);
            randomAccessFileChannel(tmp);
        } finally {
            Files.deleteIfExists(tmp);
        }
    }

    // -- 1 -- an ordinary open channel is open, and stays open across every
    //         operation. This is the over-correction guard.
    static void openDirection(Path p) {
        System.out.println("== 1 open direction ==");
        try (FileChannel fc = FileChannel.open(p, StandardOpenOption.READ)) {
            System.out.println("  class                   = " + fc.getClass().getName());
            System.out.println("  isOpen.fresh            = " + show(fc::isOpen));
            System.out.println("  position.fresh          = " + show(fc::position));
            System.out.println("  size                    = " + show(fc::size));
            ByteBuffer bb = ByteBuffer.allocate(4);
            System.out.println("  read                    = " + show(() -> fc.read(bb)));
            System.out.println("  isOpen.afterRead        = " + show(fc::isOpen));
            System.out.println("  position.afterRead      = " + show(fc::position));
        } catch (Throwable t) {
            System.out.println("  SECTION THREW: " + t);
        }
    }

    // -- 2 -- the closed direction. `isOpen()` must FLIP, not merely be false.
    static void closedDirection(Path p) {
        System.out.println("== 2 closed direction ==");
        try {
            FileChannel fc = FileChannel.open(p, StandardOpenOption.READ);
            System.out.println("  isOpen.beforeClose      = " + show(fc::isOpen));
            fc.close();
            System.out.println("  isOpen.afterClose       = " + show(fc::isOpen));
            System.out.println("  isOpen.afterCloseTwice  = " + show(() -> {
                fc.close(); // close() is specified idempotent
                return fc.isOpen();
            }));
            // Real JDK: an operation on a closed channel throws
            // ClosedChannelException. Printed as a value, not asserted, because
            // the synthetic fallback's throw shape is a separate question.
            System.out.println("  read.afterClose         = "
                    + show(() -> fc.read(ByteBuffer.allocate(1))));
        } catch (Throwable t) {
            System.out.println("  SECTION THREW: " + t);
        }
    }

    // -- 3 -- THE discriminating case: move the position, then ask isOpen().
    //         With the position aliasing `closed`, a non-zero position reads
    //         back as `closed == true`.
    static void positionMoves(Path p) {
        System.out.println("== 3 position moves, channel must stay open ==");
        try (FileChannel fc = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            System.out.println("  isOpen.at0              = " + show(fc::isOpen));
            fc.position(1);
            System.out.println("  isOpen.at1              = " + show(fc::isOpen));
            System.out.println("  position.at1            = " + show(fc::position));
            fc.position(7);
            System.out.println("  isOpen.at7              = " + show(fc::isOpen));
            System.out.println("  position.at7            = " + show(fc::position));
            // 1 is the value a boolean `true` reads as, so position 1 is the
            // sharpest single case; 7 proves it is not a one-value accident.
            ByteBuffer out = ByteBuffer.wrap(new byte[] {(byte) 0xAB});
            System.out.println("  write                   = " + show(() -> fc.write(out)));
            System.out.println("  isOpen.afterWrite       = " + show(fc::isOpen));
            System.out.println("  force                   = " + show(() -> {
                fc.force(true);
                return "ok";
            }));
            System.out.println("  isOpen.afterForce       = " + show(fc::isOpen));
            fc.position(0);
            System.out.println("  isOpen.backAt0          = " + show(fc::isOpen));
        } catch (Throwable t) {
            System.out.println("  SECTION THREW: " + t);
        }
    }

    // -- 4 -- the other door onto a channel. `RandomAccessFile.getChannel()`
    //         reaches the same natives by a different route, so a repair that
    //         only covered `FileChannel.open` would show here.
    static void randomAccessFileChannel(Path p) {
        System.out.println("== 4 RandomAccessFile.getChannel ==");
        try (RandomAccessFile raf = new RandomAccessFile(p.toFile(), "rw")) {
            FileChannel fc = raf.getChannel();
            System.out.println("  class                   = " + fc.getClass().getName());
            System.out.println("  isOpen.fresh            = " + show(fc::isOpen));
            System.out.println("  size                    = " + show(fc::size));
            fc.position(3);
            System.out.println("  isOpen.afterPosition    = " + show(fc::isOpen));
            System.out.println("  position                = " + show(fc::position));
            fc.close();
            System.out.println("  isOpen.afterClose       = " + show(fc::isOpen));
        } catch (Throwable t) {
            System.out.println("  SECTION THREW: " + t);
        }
    }
}

/*
HotSpot oracle — Temurin 25.0.3+9 (Windows 11), measured 2026-08-12.
`javap -version` = 25.0.3. Command: java probes/FileChannelIsOpenProbe.java

== 1 open direction ==
  class                   = sun.nio.ch.FileChannelImpl
  isOpen.fresh            = true
  position.fresh          = 0
  size                    = 12
  read                    = 4
  isOpen.afterRead        = true
  position.afterRead      = 4
== 2 closed direction ==
  isOpen.beforeClose      = true
  isOpen.afterClose       = false
  isOpen.afterCloseTwice  = false
  read.afterClose         = java.nio.channels.ClosedChannelException
== 3 position moves, channel must stay open ==
  isOpen.at0              = true
  isOpen.at1              = true
  position.at1            = 1
  isOpen.at7              = true
  position.at7            = 7
  write                   = 1
  isOpen.afterWrite       = true
  force                   = ok
  isOpen.afterForce       = true
  isOpen.backAt0          = true
== 4 RandomAccessFile.getChannel ==
  class                   = sun.nio.ch.FileChannelImpl
  isOpen.fresh            = true
  size                    = 12
  isOpen.afterPosition    = true
  position                = 3
  isOpen.afterClose       = false

Reading it. Only THREE lines are `false`, and they are all "after close". Every
other `isOpen` is `true`. That asymmetry is the assertion: a VM that answers
`true` uniformly fails §2 and §4, a VM that answers `false` uniformly fails §1
and §3, and only one that tracks the transition matches. The old CratonVM body
for the literal synthetic class returned a constant `1`, i.e. it failed §2's
`isOpen.afterClose` and §4's; a naive renumber of the private slot map that
left `isOpen()` reading slot 0 as an fd would have failed §1 and §3 instead.

`class` lines are INFORMATIONAL. In real-JDK mode CratonVM routes
`FileChannel.open` and `RandomAccessFile.getChannel` to a genuine
`sun.nio.ch.FileChannelImpl` (the `newFileChannel` shim's RECONCILE-WITH-REAL
block), so they should match; a `java.nio.channels.FileChannel` there means the
legacy synthetic fallback fired, which is the object this repair is about, and
the rest of the transcript is then the interesting part.

`position.*` values are printed rather than asserted because the synthetic
fallback's position tracking is a separate question from `isOpen()`; they
matter here only as evidence that the position actually moved, which is what
made the old `closed` alias fire.
*/
