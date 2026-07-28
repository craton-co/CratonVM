/*
 * Regression probe for the JIT entry wrapper of ACC_SYNCHRONIZED methods.
 * It covers instance/static lock identity, a caught exception, and contended
 * calls. A failure is deliberately loud: an incorrectly released monitor can
 * otherwise look like a harmless performance regression.
 */
public final class SynchronizedJitContractProbe {
    private static final class Holder { int value; }
    private int instanceCount;
    private static int staticCount;

    private synchronized int instanceStep() {
        if (!Thread.holdsLock(this)) {
            throw new AssertionError("instance monitor was not held");
        }
        return ++instanceCount;
    }

    private static synchronized int staticStep() {
        if (!Thread.holdsLock(SynchronizedJitContractProbe.class)) {
            throw new AssertionError("class monitor was not held");
        }
        return ++staticCount;
    }

    private synchronized int caughtExceptionKeepsLock() {
        try {
            throw new IllegalStateException("expected");
        } catch (IllegalStateException expected) {
            if (!Thread.holdsLock(this)) {
                throw new AssertionError("catch handler lost instance monitor");
            }
            return 17;
        }
    }

    private synchronized int propagatedExceptionKeepsMonitorUntilExit() {
        if (!Thread.holdsLock(this)) {
            throw new AssertionError("throw path entered without monitor");
        }
        throw new IllegalArgumentException("expected");
    }

    private void synchronizedBlockNullPutfield(Holder holder) {
        synchronized (this) {
            holder.value = 1;
        }
    }

    public static void main(String[] args) throws Exception {
        SynchronizedJitContractProbe probe = new SynchronizedJitContractProbe();
        for (int i = 0; i < 12_000; i++) {
            probe.instanceStep();
            staticStep();
            if (probe.caughtExceptionKeepsLock() != 17) {
                throw new AssertionError("wrong catch result");
            }
        }
        for (int i = 0; i < 128; i++) {
            try {
                probe.propagatedExceptionKeepsMonitorUntilExit();
                throw new AssertionError("expected exception was not thrown");
            } catch (IllegalArgumentException expected) {
                if (Thread.holdsLock(probe)) {
                    throw new AssertionError("monitor remained held after propagation");
                }
            }
        }
        Holder holder = new Holder();
        for (int i = 0; i < 12_000; i++) {
            probe.synchronizedBlockNullPutfield(holder);
        }
        for (int i = 0; i < 128; i++) {
            try {
                probe.synchronizedBlockNullPutfield(null);
                throw new AssertionError("expected null putfield exception was not thrown");
            } catch (NullPointerException expectedException) {
                if (Thread.holdsLock(probe)) {
                    throw new AssertionError("block monitor remained held after null putfield");
                }
            }
        }

        Thread[] workers = new Thread[4];
        for (int t = 0; t < workers.length; t++) {
            workers[t] = new Thread(() -> {
                for (int i = 0; i < 4_000; i++) {
                    probe.instanceStep();
                }
            });
            workers[t].start();
        }
        for (Thread worker : workers) {
            worker.join();
        }
        int expected = 12_000 + workers.length * 4_000;
        if (probe.instanceCount != expected || staticCount != 12_000) {
            throw new AssertionError("lost update: instance=" + probe.instanceCount
                + " static=" + staticCount);
        }
        System.out.println("SYNC_JIT_CONTRACT_OK instance=" + probe.instanceCount
            + " static=" + staticCount);
    }
}
