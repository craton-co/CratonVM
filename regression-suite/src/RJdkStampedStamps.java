import java.util.concurrent.locks.StampedLock;

/**
 * JDK-only corpus: the BITS of a {@code StampedLock} stamp, as decoded by the
 * JDK's own unregistered static predicates.
 *
 * {@code isWriteLockStamp}, {@code isReadLockStamp}, {@code isLockStamp} and
 * {@code isOptimisticReadStamp} are {@code public static boolean (long)} —
 * pure functions of the stamp that touch no instance state — and no registrar
 * provides them. REAL JDK BYTECODE therefore decodes whatever {@code long} the
 * VM's lock backend handed out, in the JDK's own layout:
 *
 * <pre>
 *   LG_READERS = 7   WBIT = 128   RBITS = 127   ABITS = 255   ORIGIN = 256
 *   stamp &amp; ABITS == WBIT    write stamp
 *   stamp &amp; ABITS == 0       optimistic stamp (0 itself is invalid)
 *   stamp &amp; ABITS in 1..126  read stamp, the value being the reader COUNT
 * </pre>
 *
 * A backend that picks its own flags — {@code version|1} for write and
 * {@code version|2} for read was CratonVM's encoding until 2026-08-07 — makes
 * {@code isWriteLockStamp(writeLock())} answer {@code false} and
 * {@code isReadLockStamp(writeLock())} answer {@code true}, because
 * {@code version|1} decodes as "read lock held by one reader". That sends the
 * canonical release idiom
 *
 * <pre>
 *   if (isWriteLockStamp(s)) lock.unlockWrite(s); else lock.unlockRead(s);
 * </pre>
 *
 * down the wrong branch: {@code unlockRead} raises
 * {@code IllegalMonitorStateException} or silently does nothing, the write hold
 * is never dropped, and the next {@code readLock()} blocks forever. Nothing in
 * the lock's own registered surface can see that — every registered method
 * agrees with itself. Only an unregistered decoder can, which is what this
 * vector is.
 *
 * WHY THIS IS NOT COVERED BY RJdkAqs: that vector asks {@code isWriteLocked()}
 * and {@code isReadLocked()}, which ARE registered, so they answer from the
 * side table and agree with themselves no matter what the stamps say. The whole
 * defect lives in the gap between the two.
 *
 * MODE NOTE. Under {@code --jdk-only} the {@code StampedLock} surface is
 * {@code SyntheticStub} and is refused at registration, so the real class runs
 * end to end; under {@code --real-jdk} the native backend runs and the
 * predicates decode ITS stamps. The assertions below are the same in both
 * modes on purpose — that is the point of matching the JDK's layout, and a
 * divergence between the two modes is itself the finding.
 *
 * Determinism: single-threaded, and no absolute stamp value is printed except
 * the mode field ({@code stamp &amp; 255}) and the ORIGIN-relative version step,
 * both of which the JDK fixes exactly.
 */
public class RJdkStampedStamps {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RJdkStampedStamps: " + m);
        }
    }

    /** A write stamp decodes as a write stamp, and as nothing else. */
    static void writeStampBits() {
        StampedLock l = new StampedLock();
        long w = l.writeLock();
        check((w & 255L) == 128L, "writeLock() mode field must be WBIT=128, got " + (w & 255L));
        check(StampedLock.isWriteLockStamp(w), "isWriteLockStamp(writeLock()) must be true");
        check(!StampedLock.isReadLockStamp(w), "isReadLockStamp(writeLock()) must be FALSE");
        check(StampedLock.isLockStamp(w), "isLockStamp(writeLock()) must be true");
        check(!StampedLock.isOptimisticReadStamp(w),
                "isOptimisticReadStamp(writeLock()) must be false");
        check(l.validate(w), "a held write stamp validates");
        l.unlockWrite(w);
        check(!l.isWriteLocked(), "unlockWrite released the hold");
        System.out.println("CK RJdkStampedStamps writeMode=" + (w & 255L));
    }

    /** A read stamp's low bits are the reader COUNT, not a flag. */
    static void readStampBits() {
        StampedLock l = new StampedLock();
        long r1 = l.readLock();
        check((r1 & 255L) == 1L, "first readLock() mode field must be 1, got " + (r1 & 255L));
        check(StampedLock.isReadLockStamp(r1), "isReadLockStamp(readLock()) must be true");
        check(!StampedLock.isWriteLockStamp(r1), "isWriteLockStamp(readLock()) must be false");
        check(StampedLock.isLockStamp(r1), "isLockStamp(readLock()) must be true");
        check(!StampedLock.isOptimisticReadStamp(r1), "a read stamp is not optimistic");
        long r2 = l.readLock();
        check((r2 & 255L) == 2L, "second concurrent readLock() counts 2, got " + (r2 & 255L));
        check(r2 - r1 == 1L, "two concurrent read stamps differ by RUNIT=1");
        check(l.getReadLockCount() == 2, "two read holds outstanding");
        l.unlockRead(r2);
        l.unlockRead(r1);
        check(l.getReadLockCount() == 0, "both read holds released");
        System.out.println("CK RJdkStampedStamps readModes=" + (r1 & 255L) + "," + (r2 & 255L));
    }

    /** An optimistic stamp has an EMPTY mode field and is not a lock stamp. */
    static void optimisticStampBits() {
        StampedLock l = new StampedLock();
        long o = l.tryOptimisticRead();
        check(o != 0L, "a free lock hands out a non-zero optimistic stamp");
        check((o & 255L) == 0L, "optimistic stamp mode field must be 0, got " + (o & 255L));
        check(StampedLock.isOptimisticReadStamp(o), "isOptimisticReadStamp must be true");
        check(!StampedLock.isLockStamp(o), "an optimistic stamp holds no lock");
        check(!StampedLock.isWriteLockStamp(o) && !StampedLock.isReadLockStamp(o),
                "an optimistic stamp is neither a read nor a write stamp");
        check(l.validate(o), "nothing has run, so the observation is still valid");
        // 0 is the invalid sentinel and must not decode as any mode.
        check(!StampedLock.isOptimisticReadStamp(0L), "0 is not an optimistic stamp");
        check(!StampedLock.isLockStamp(0L), "0 is not a lock stamp");
        check(!l.validate(0L), "validate(0) is false");
        // A writer invalidates an outstanding observation.
        long w = l.writeLock();
        check(!l.validate(o), "a write invalidates the earlier observation");
        check(l.tryOptimisticRead() == 0L, "tryOptimisticRead under a writer returns 0");
        l.unlockWrite(w);
        System.out.println("CK RJdkStampedStamps optMode=" + (o & 255L));
    }

    /**
     * The version arithmetic the predicates share with {@code validate}: one
     * full write lock/unlock cycle advances the observation stamp by exactly
     * {@code 2 * WBIT == 256}, so the low 8 bits stay the mode field.
     */
    static void versionStep() {
        StampedLock l = new StampedLock();
        long o0 = l.tryOptimisticRead();
        long w = l.writeLock();
        l.unlockWrite(w);
        long o1 = l.tryOptimisticRead();
        check(o1 - o0 == 256L, "one write cycle advances the version by 256, got " + (o1 - o0));
        check((o1 & 255L) == 0L, "the advanced stamp is still optimistic");
        check(!l.validate(o0), "the pre-write observation no longer validates");
        System.out.println("CK RJdkStampedStamps versionStep=" + (o1 - o0));
    }

    /**
     * The idiom the whole encoding exists to serve. Releasing through the
     * predicates must actually release, in both branches, or the following
     * acquisition deadlocks.
     */
    static void releaseIdiom() {
        StampedLock l = new StampedLock();
        for (int i = 0; i < 3; i++) {
            long s = (i % 2 == 0) ? l.writeLock() : l.readLock();
            if (StampedLock.isWriteLockStamp(s)) {
                l.unlockWrite(s);
            } else if (StampedLock.isReadLockStamp(s)) {
                l.unlockRead(s);
            } else {
                check(false, "a lock stamp decoded as neither read nor write");
            }
            check(!l.isWriteLocked(), "round " + i + ": write hold leaked");
            check(l.getReadLockCount() == 0, "round " + i + ": read hold leaked");
        }
        // If any hold had leaked, this would block forever rather than fail.
        long w = l.tryWriteLock();
        check(w != 0L, "the lock is free after three predicate-driven releases");
        l.unlockWrite(w);
        System.out.println("CK RJdkStampedStamps releaseIdiom=ok");
    }

    /**
     * {@code unlock(long)} dispatches on the stamp too, and its JDK arm is the
     * WEAKER test {@code (stamp & WBIT) != 0}. It must agree with the
     * predicates about which hold it is dropping.
     */
    static void unlockDispatch() {
        StampedLock l = new StampedLock();
        long w = l.writeLock();
        l.unlock(w);
        check(!l.isWriteLocked(), "unlock(writeStamp) dropped the write hold");
        long r = l.readLock();
        l.unlock(r);
        check(l.getReadLockCount() == 0, "unlock(readStamp) dropped the read hold");
        System.out.println("CK RJdkStampedStamps unlockDispatch=ok");
    }

    /**
     * Conversions hand back stamps in the same layout, so the predicates must
     * classify a converted stamp correctly too.
     */
    static void convertedStamps() {
        StampedLock l = new StampedLock();
        long w = l.writeLock();
        long r = l.tryConvertToReadLock(w);
        check(r != 0L, "write -> read downgrade succeeds");
        check(StampedLock.isReadLockStamp(r), "the downgraded stamp is a read stamp");
        check(!StampedLock.isWriteLockStamp(r), "the downgraded stamp is not a write stamp");
        check(!l.isWriteLocked() && l.getReadLockCount() == 1, "downgraded state");
        long w2 = l.tryConvertToWriteLock(r);
        check(w2 != 0L, "sole read hold upgrades to write");
        check(StampedLock.isWriteLockStamp(w2), "the upgraded stamp is a write stamp");
        long o = l.tryConvertToOptimisticRead(w2);
        check(o != 0L, "converting a held write stamp to an observation succeeds");
        check(StampedLock.isOptimisticReadStamp(o), "the result is an optimistic stamp");
        check(!l.isWriteLocked() && l.getReadLockCount() == 0,
                "tryConvertToOptimisticRead is a RELEASE path");
        System.out.println("CK RJdkStampedStamps converted=" + (o & 255L));
    }

    public static void main(String[] args) {
        writeStampBits();
        readStampBits();
        optimisticStampBits();
        versionStep();
        releaseIdiom();
        unlockDispatch();
        convertedStamps();
        System.out.println("CK RJdkStampedStamps checks=" + checks);
        System.out.println("PASS RJdkStampedStamps (" + checks + " checks)");
    }
}
