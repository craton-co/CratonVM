import io.netty.util.Recycler;
import io.netty.util.concurrent.FastThreadLocalThread;

import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;

/**
 * Which link of netty's Recycler keeps a finished Thread alive on CratonVM?
 *
 * `RecyclerTest.testThreadCanBeCollectedEvenIfHandledObjectIsReferenced` times
 * out for 4 of its 6 parameterisations here and 0 of 6 on HotSpot. This runs
 * the same shape with pieces of the thread body removed, so the retaining link
 * is named rather than guessed:
 *
 *   full     — create Recycler, get() an object, KEEP it, unpinOwner   (the test)
 *   nokeep   — same but the object is dropped
 *   noget    — Recycler created, get() never called
 *   nopin    — the test's body without the unpinOwner call
 *   bare     — no Recycler at all (control; must always collect)
 *
 * x {NONE, PINNED, FAST_THREAD_LOCAL} x {unguarded, guarded}.
 */
public final class RecyclerRetainProbe {

    static final int TIMEOUT_MS = 3000;

    static final class Handled {
        final Recycler.Handle<Handled> handle;
        Handled(Recycler.Handle<Handled> handle) { this.handle = handle; }
    }

    enum Owner { NONE, PINNED, FAST_THREAD_LOCAL }

    static Recycler<Handled> newRecycler(Owner owner, boolean unguarded) {
        switch (owner) {
            case NONE:
                return new Recycler<Handled>(0, unguarded) {
                    @Override protected Handled newObject(Recycler.Handle<Handled> h) { return new Handled(h); }
                };
            case PINNED:
                return new Recycler<Handled>(Thread.currentThread(), unguarded) {
                    @Override protected Handled newObject(Recycler.Handle<Handled> h) { return new Handled(h); }
                };
            default:
                return new Recycler<Handled>(unguarded) {
                    @Override protected Handled newObject(Recycler.Handle<Handled> h) { return new Handled(h); }
                };
        }
    }

    static final class Watched extends Thread {
        final AtomicBoolean flag;
        Watched(AtomicBoolean f, Runnable r) { super(r); flag = f; }
        @Override protected void finalize() { flag.set(true); }
    }

    static long await(AtomicBoolean flag) throws InterruptedException {
        long deadline = System.nanoTime() + TIMEOUT_MS * 1_000_000L;
        long t0 = System.nanoTime();
        while (!flag.get()) {
            if (System.nanoTime() > deadline) {
                return -1;
            }
            System.gc();
            System.runFinalization();
            Thread.sleep(50);
        }
        return (System.nanoTime() - t0) / 1_000_000L;
    }

    static String run(Owner owner, boolean unguarded, String variant) throws Exception {
        final AtomicBoolean collected = new AtomicBoolean();
        final AtomicReference<Handled> reference = new AtomicReference<>();
        Thread t = new Watched(collected, () -> {
            if ("bare".equals(variant)) {
                return;
            }
            Recycler<Handled> recycler = newRecycler(owner, unguarded);
            if (!"noget".equals(variant)) {
                Handled object = recycler.get();
                if (!"nokeep".equals(variant)) {
                    reference.set(object);
                }
            }
            if (!"nopin".equals(variant)) {
                Recycler.unpinOwner(recycler);
            }
        });
        t.start();
        t.join();
        t = null;
        long ms = await(collected);
        // Touch `reference` after the wait so it stays live across it, exactly
        // as the test's field does.
        boolean held = reference.get() != null;
        return (ms < 0 ? "RETAINED" : ms + "ms") + (held ? "" : " (obj dropped)");
    }

    public static void main(String[] args) throws Exception {
        System.out.println("FastThreadLocalThread.currentThreadWillCleanupFastThreadLocals()="
                           + FastThreadLocalThread.currentThreadWillCleanupFastThreadLocals());
        String[] variants = { "bare", "noget", "nokeep", "nopin", "full" };
        System.out.printf("%-28s", "owner/guard");
        for (String v : variants) {
            System.out.printf("%-22s", v);
        }
        System.out.println();
        for (Owner o : Owner.values()) {
            for (boolean unguarded : new boolean[] { true, false }) {
                System.out.printf("%-28s", o + "/" + (unguarded ? "unguarded" : "guarded"));
                for (String v : variants) {
                    System.out.printf("%-22s", run(o, unguarded, v));
                }
                System.out.println();
            }
        }
        System.exit(0);
    }
}
