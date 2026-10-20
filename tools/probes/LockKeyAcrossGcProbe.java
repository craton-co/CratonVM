import java.util.concurrent.locks.ReentrantReadWriteLock;
import java.util.concurrent.locks.StampedLock;

/**
 * gc-common w14-c: the lock-key registry (native StampedLock / RRWL state is
 * filed under a GC-stable key) is now swept after every collection and
 * re-addressed from the pointer map. Three locks are held across collections
 * that churn thousands of short-lived locks (whose keys are swept) and move
 * the survivors; each held lock must still read as held, still exclude
 * another thread, and unlock cleanly afterwards. Prints one line per round and
 * a final verdict; the output should match HotSpot's.
 *
 * Ends on its own. Arg 0: short-lived locks per round (default 20000).
 */
public final class LockKeyAcrossGcProbe {
	static volatile Object sink;

	public static void main(String[] a) throws Exception {
		int n = a.length > 0 ? Integer.parseInt(a[0]) : 20000;
		StampedLock written = new StampedLock();
		long ws = written.writeLock();
		StampedLock read = new StampedLock();
		long rs = read.readLock();
		ReentrantReadWriteLock rw = new ReentrantReadWriteLock();
		rw.writeLock().lock();
		int bad = 0;
		for (int round = 1; round <= 5; round++) {
			churn(n);
			System.gc();
			int before = bad;
			if (!written.isWriteLocked()) bad++;
			if (written.tryReadLock() != 0L) bad++;
			if (read.getReadLockCount() != 1) bad++;
			if (read.tryWriteLock() != 0L) bad++;
			if (!rw.isWriteLockedByCurrentThread()) bad++;
			boolean[] stolen = new boolean[1];
			Thread t = new Thread(() -> stolen[0] = rw.writeLock().tryLock() || rw.readLock().tryLock());
			t.start();
			t.join();
			if (stolen[0]) bad++;
			System.out.println("round " + round + ": " + (bad == before ? "held locks intact" : "LOST " + (bad - before)));
		}
		written.unlockWrite(ws);
		read.unlockRead(rs);
		rw.writeLock().unlock();
		if (written.isWriteLocked() || written.isReadLocked() || read.isWriteLocked() || read.isReadLocked() || rw.isWriteLocked()) bad++;
		long s = written.tryWriteLock();
		if (s == 0L) bad++;
		else written.unlockWrite(s);
		System.out.println(bad == 0 ? "LockKeyAcrossGcProbe: OK" : "LockKeyAcrossGcProbe: FAIL bad=" + bad);
	}

	/** Short-lived locks, each keyed once and then dropped, plus garbage. */
	static void churn(int n) {
		for (int i = 0; i < n; i++) {
			StampedLock x = new StampedLock();
			long s = x.writeLock();
			x.unlockWrite(s);
			ReentrantReadWriteLock y = new ReentrantReadWriteLock();
			y.readLock().lock();
			y.readLock().unlock();
			sink = new byte[48];
		}
	}
}
