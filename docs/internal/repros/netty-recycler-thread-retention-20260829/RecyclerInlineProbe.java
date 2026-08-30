import io.netty.util.Recycler;

import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicReference;

/**
 * The combination neither earlier probe covered.
 *
 * `RecyclerRetainProbe` ran the netty test's body but put the gc-loop in a
 * CALLEE, and every cell collected. `FrameRetainProbe` put the gc-loop INLINE
 * but with a bare Thread, and every arm collected. The netty test does both:
 * a Recycler-allocating thread body AND the loop inline in the frame that held
 * the Thread. This is that cell.
 *
 * Byte-for-byte the shape of
 * `RecyclerTest.testThreadCanBeCollectedEvenIfHandledObjectIsReferenced`,
 * minus JUnit. Run with and without CRATONVM_BG_COMPILE=0.
 */
public final class RecyclerInlineProbe {

    static final int TIMEOUT_MS = 4000;

    static final class Handled {
        final Recycler.Handle<Handled> handle;
        Handled(Recycler.Handle<Handled> handle) { this.handle = handle; }
    }

    enum Owner { NONE, PINNED, FAST_THREAD_LOCAL }

    static Recycler<Handled> newRecycler(Owner owner, boolean unguarded) {
        switch (owner) {
            case NONE:
                return new Recycler<Handled>(1024, unguarded) {
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

    /** The test's body, loop included, all in one frame. */
    static long run(Owner owner, boolean unguarded) throws Exception {
        final AtomicBoolean collected = new AtomicBoolean();
        final AtomicReference<Handled> reference = new AtomicReference<>();
        Thread thread = new Watched(collected, () -> {
            Recycler<Handled> recycler = newRecycler(owner, unguarded);
            Handled object = recycler.get();
            reference.set(object);
            Recycler.unpinOwner(recycler);
        });
        thread.start();
        thread.join();
        thread = null;

        long t0 = System.nanoTime();
        long deadline = t0 + TIMEOUT_MS * 1_000_000L;
        while (!collected.get()) {
            if (System.nanoTime() > deadline) {
                return -1;
            }
            System.gc();
            System.runFinalization();
            Thread.sleep(50);
        }
        if (reference.get() != null) {
            reference.getAndSet(null);
        }
        return (System.nanoTime() - t0) / 1_000_000L;
    }

    public static void main(String[] args) throws Exception {
        for (int round = 1; round <= 2; round++) {
            System.out.println("round " + round + ":");
            for (Owner o : Owner.values()) {
                for (boolean unguarded : new boolean[] { true, false }) {
                    long ms = run(o, unguarded);
                    System.out.printf("  %-34s %s%n", o + "/" + (unguarded ? "unguarded" : "guarded"),
                                      ms < 0 ? "RETAINED (" + TIMEOUT_MS + "ms)" : "collected in " + ms + "ms");
                }
            }
        }
        System.exit(0);
    }
}
