/**
 * Reproduce residual 2's `result=Some(Int(0))` on demand, in one run.
 *
 * `known-issues/netty/parameterizedsslhandlertest-residual-stalls` records a
 * stall whose `io.netty.util.concurrent.DefaultPromise.result` — declared
 * `private volatile Object` — dumped as `Int(0)`, and states that whether that
 * is the CAUSE or an artefact was never established. This probe settles it
 * without a stall and without netty: allocate the same shape from a HOT method
 * so the JIT's allocation arms serve it, never write the reference field, and
 * park on the object so the watchdog dumps it.
 *
 * If the dump says `Int(0)`, a never-written reference field of a
 * JIT-allocated object is what that looks like — the interpreter's allocation
 * path writes the `Object(None)` tag, the JIT's clears the body to zero, and a
 * zero-filled 16-byte `Value` cell decodes as `Int(0)` because `Int` is
 * discriminant 0. Every other reader in the VM repairs it; the watchdog dump
 * was the one that did not.
 *
 * The `--nojit` arm is the control: same probe, same object, interpreter
 * allocation, and the dump must read `Object(None)`.
 */
public final class JitZeroCellProbe {
    /** `DefaultPromise`'s two load-bearing fields, same names and types. */
    static final class FakePromise {
        private volatile Object result;
        @SuppressWarnings("unused")
        private short waiters;
    }

    /** Kept out of `main` so it tiers up as an ordinary hot callee. */
    private static FakePromise make() {
        return new FakePromise();
    }

    private static Object sink;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        FakePromise keep = null;
        for (int i = 0; i < rounds; i++) {
            FakePromise p = make();
            // Touch it so the allocation cannot be scalar-replaced away, but
            // NEVER write `result` — the whole point is an unwritten slot.
            sink = p;
            if (i == rounds - 1) {
                keep = p;
            }
        }
        System.out.println("PROBE: allocated " + rounds + " promises; last one is "
                + (keep == null ? "null" : "live") + "; result==null is "
                + (keep != null && keep.result == null));
        System.out.flush();
        final Object lock = keep;
        Thread t = new Thread(() -> {
            synchronized (lock) {
                while (true) {
                    try {
                        lock.wait();
                    } catch (InterruptedException e) {
                        return;
                    }
                }
            }
        }, "jit-allocated-promise-waiter");
        t.setDaemon(true);
        t.start();
        Thread.sleep(600_000L);
    }
}
