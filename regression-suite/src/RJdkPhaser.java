import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Phaser;
import java.util.concurrent.TimeUnit;

/**
 * JDK-only corpus: {@code java.util.concurrent.Phaser} -- real bytecode, real
 * field state.
 *
 * Every method of the real {@code Phaser} has Code; the class keeps its whole
 * world in one {@code volatile long state} (phase in the high 32 bits, parties
 * in bits 16-31, unarrived in bits 0-15) plus {@code parent}/{@code root}/
 * {@code evenQ}/{@code oddQ}. A native shim that mirrors that into its own
 * side-state -- a 3-int holder, three object slots, anything -- is a SPLIT
 * BRAIN: one half of the API reads the shim and the other half reads the real
 * long, and they disagree.
 *
 * This vector is written so that a split brain cannot pass it. It does not ask
 * "does arrive() roughly work"; it pins the values a hand-rolled model gets
 * wrong, each of which is a distinct, measured HotSpot 25 fact:
 *
 *   * {@code arriveAndDeregister()} lowers parties AND unarrived together, so
 *     a phaser of 2 with nobody arrived does NOT advance when one party
 *     deregisters. A model that counts "arrived" instead of "unarrived"
 *     advances here, one arrival early.
 *   * Termination is encoded IN the phase: the terminated phase is
 *     {@code phase | Integer.MIN_VALUE}, not {@code -1}. The exact bit
 *     patterns are asserted, so a sentinel-based model fails on the value even
 *     when {@code isTerminated()} happens to agree.
 *   * A terminated phaser STAYS terminated. {@code arrive()} on it returns the
 *     negative phase and must not advance anything -- a model that keeps
 *     counting arrivals rolls {@code -1} up to {@code 0} and silently
 *     resurrects the phaser.
 *   * Arriving at a phaser with no registered parties throws
 *     {@code IllegalStateException}, it does not return quietly.
 *   * A tiered phaser registers exactly ONE party with its parent, and only
 *     once it has parties of its own.
 *
 * Determinism: the only multi-threaded section rendezvouses on the phaser
 * itself and joins with a bounded timeout; no sleeps order anything asserted,
 * and no identity hash codes or thread names are printed (the {@code toString}
 * check prints the bracketed tail only).
 */
public class RJdkPhaser {
    static final long T = 30;
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RJdkPhaser: " + m);
        }
    }

    static void state(Phaser p, int phase, int reg, int arrived, int unarrived,
            boolean terminated, String where) {
        check(p.getPhase() == phase, where + ": phase=" + p.getPhase() + " want " + phase);
        check(p.getRegisteredParties() == reg,
                where + ": registered=" + p.getRegisteredParties() + " want " + reg);
        check(p.getArrivedParties() == arrived,
                where + ": arrived=" + p.getArrivedParties() + " want " + arrived);
        check(p.getUnarrivedParties() == unarrived,
                where + ": unarrived=" + p.getUnarrivedParties() + " want " + unarrived);
        check(p.isTerminated() == terminated,
                where + ": terminated=" + p.isTerminated() + " want " + terminated);
    }

    /** Counters on a fresh phaser, and the arrival phase arrive() reports. */
    static void countersAndArrive() {
        state(new Phaser(), 0, 0, 0, 0, false, "new Phaser()");
        Phaser p = new Phaser(3);
        state(p, 0, 3, 0, 3, false, "new Phaser(3)");

        // arrive() returns the phase it arrived AT, never the phase it caused.
        check(p.arrive() == 0, "arrive #1 must return arrival phase 0");
        state(p, 0, 3, 1, 2, false, "after arrive #1");
        check(p.arrive() == 0, "arrive #2 must return arrival phase 0");
        state(p, 0, 3, 2, 1, false, "after arrive #2");
        check(p.arrive() == 0, "the advancing arrive must STILL return phase 0");
        state(p, 1, 3, 0, 3, false, "after the advance");
        check(p.arrive() == 1, "arrive in phase 1 must return 1");
        state(p, 1, 3, 1, 2, false, "phase 1, one arrived");
        System.out.println("CK RJdkPhaser arrive phase=" + p.getPhase()
                + " arrived=" + p.getArrivedParties());
    }

    /** register()/bulkRegister() return the current phase and add unarrived parties. */
    static void registration() {
        Phaser p = new Phaser(1);
        check(p.register() == 0, "register() must return the current phase");
        state(p, 0, 2, 0, 2, false, "after register");
        check(p.bulkRegister(3) == 0, "bulkRegister(3) must return the current phase");
        state(p, 0, 5, 0, 5, false, "after bulkRegister(3)");
        check(p.bulkRegister(0) == 0, "bulkRegister(0) must return the current phase");
        state(p, 0, 5, 0, 5, false, "bulkRegister(0) is a no-op");

        // Registering mid-phase adds an UNARRIVED party; the arrived count of
        // the parties already in this phase is untouched.
        check(p.arrive() == 0, "mid-phase arrive");
        state(p, 0, 5, 1, 4, false, "one of five arrived");
        check(p.register() == 0, "mid-phase register() returns the running phase");
        state(p, 0, 6, 1, 5, false, "register mid-phase keeps arrived, raises unarrived");
        System.out.println("CK RJdkPhaser register reg=" + p.getRegisteredParties()
                + " unarrived=" + p.getUnarrivedParties());
    }

    /**
     * arriveAndDeregister() drops parties and unarrived TOGETHER. This is the
     * check a side-state model fails: from (parties=2, unarrived=2) one
     * deregistration leaves (1, 1) and the phase does NOT move.
     */
    static void deregister() {
        Phaser p = new Phaser(2);
        check(p.arriveAndDeregister() == 0, "arriveAndDeregister returns the arrival phase");
        state(p, 0, 1, 0, 1, false,
                "deregistering one of two must NOT advance the phase");

        // The last party leaving completes the phase (unarrived hits 0) and the
        // default onAdvance terminates on zero registered parties. The phase
        // advances FIRST, so the terminated phase is 1 with the sign bit set.
        check(p.arriveAndDeregister() == 0, "the last deregistration still reports phase 0");
        state(p, Integer.MIN_VALUE | 1, 0, 0, 0, true, "terminated by the last deregistration");
        check(p.getPhase() == -2147483647, "terminated phase must be 0x80000001, got "
                + Integer.toHexString(p.getPhase()));

        // A terminated phaser is inert: register() reports the negative phase
        // and does not take a new party.
        check(p.register() == -2147483647, "register() on a terminated phaser returns its phase");
        state(p, Integer.MIN_VALUE | 1, 0, 0, 0, true, "register on a terminated phaser is inert");

        // Same walk from three parties: two intermediate steps, no advance in
        // either. A model that advances when arrived+1 >= parties terminates
        // this phaser on the SECOND call.
        Phaser q = new Phaser(3);
        check(q.arriveAndDeregister() == 0, "3->2 returns phase 0");
        state(q, 0, 2, 0, 2, false, "3 parties -> 2, still phase 0");
        check(q.arriveAndDeregister() == 0, "2->1 returns phase 0");
        state(q, 0, 1, 0, 1, false, "2 parties -> 1, still phase 0");
        check(q.arriveAndDeregister() == 0, "1->0 returns phase 0");
        state(q, Integer.MIN_VALUE | 1, 0, 0, 0, true, "1 party -> 0 terminates");
        System.out.println("CK RJdkPhaser deregister terminatedPhase=0x"
                + Integer.toHexString(q.getPhase()));
    }

    /** Arriving without being registered is an error, not a quiet no-op. */
    static void unregisteredArrival() {
        Phaser p = new Phaser();
        String m1 = null;
        try {
            p.arrive();
        } catch (IllegalStateException e) {
            m1 = e.getMessage();
        }
        check(m1 != null, "arrive() with 0 registered parties must throw IllegalStateException");
        check(m1.startsWith("Attempted arrival of unregistered party"),
                "IllegalStateException message: " + m1);
        String m2 = null;
        try {
            p.arriveAndDeregister();
        } catch (IllegalStateException e) {
            m2 = e.getMessage();
        }
        check(m2 != null,
                "arriveAndDeregister() with 0 registered parties must throw IllegalStateException");
        check(m2.startsWith("Attempted arrival of unregistered party"),
                "IllegalStateException message: " + m2);
        state(p, 0, 0, 0, 0, false, "a refused arrival must not change the phaser");
        System.out.println("CK RJdkPhaser unregisteredArrival=IllegalStateException");
    }

    /** arriveAndAwaitAdvance() returns the NEXT phase, to every waiter. */
    static void awaitAdvanceThreads() throws Exception {
        final int n = 3;
        final Phaser p = new Phaser(n);
        final int[] rets = new int[n];
        final CountDownLatch done = new CountDownLatch(n);
        List<Thread> ts = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            final int idx = i;
            Thread t = new Thread(() -> {
                rets[idx] = p.arriveAndAwaitAdvance();
                done.countDown();
            });
            t.setDaemon(true);
            ts.add(t);
            t.start();
        }
        check(done.await(T, TimeUnit.SECONDS),
                "arriveAndAwaitAdvance must release all parties at the advance");
        for (Thread t : ts) {
            t.join(T * 1000);
            check(!t.isAlive(), "a party never returned from arriveAndAwaitAdvance");
        }
        for (int i = 0; i < n; i++) {
            check(rets[i] == 1, "arriveAndAwaitAdvance must return the NEW phase 1, got " + rets[i]);
        }
        state(p, 1, n, 0, n, false, "after the barrier advance");
        System.out.println("CK RJdkPhaser awaitAdvance rets=" + rets[0] + "," + rets[1]
                + "," + rets[2]);
    }

    /** awaitAdvance() on a negative or already-past phase returns immediately. */
    static void awaitAdvancePastPhase() {
        Phaser p = new Phaser(1);
        check(p.awaitAdvance(-1) == -1, "awaitAdvance(negative) returns its argument");
        check(p.arrive() == 0, "single-party arrive advances");
        check(p.awaitAdvance(0) == 1, "awaitAdvance() on a completed phase returns the new phase");
        state(p, 1, 1, 0, 1, false, "after the single-party advance");
        System.out.println("CK RJdkPhaser awaitAdvancePastPhase=ok");
    }

    /** An onAdvance override is real bytecode on a real subclass; it must run. */
    static void onAdvanceTermination() {
        final List<String> calls = new ArrayList<>();
        Phaser p = new Phaser(2) {
            @Override
            protected boolean onAdvance(int phase, int registeredParties) {
                calls.add(phase + "/" + registeredParties);
                return phase >= 1;
            }
        };
        p.arrive();
        check(p.arrive() == 0, "phase 0 completes and reports phase 0");
        state(p, 1, 2, 0, 2, false, "onAdvance(0,2) returned false: phase 1");

        p.arrive();
        check(p.arrive() == 1, "phase 1 completes and reports phase 1");
        // onAdvance returning true terminates: the phase advances to 2 and the
        // sign bit is set. The counters FREEZE at the completing phase's values.
        check(p.getPhase() == (Integer.MIN_VALUE | 2),
                "terminated phase must be 0x80000002, got " + Integer.toHexString(p.getPhase()));
        state(p, Integer.MIN_VALUE | 2, 2, 2, 0, true, "terminated by onAdvance");

        // Terminated means terminated: arriving neither advances nor re-runs
        // onAdvance.
        check(p.arrive() == (Integer.MIN_VALUE | 2),
                "arrive() on a terminated phaser returns its negative phase");
        check(p.arrive() == (Integer.MIN_VALUE | 2), "and stays there");
        state(p, Integer.MIN_VALUE | 2, 2, 2, 0, true, "a terminated phaser does not resurrect");
        check(calls.size() == 2, "onAdvance must run exactly twice, ran " + calls.size());
        check(calls.get(0).equals("0/2"), "onAdvance call 1: " + calls.get(0));
        check(calls.get(1).equals("1/2"), "onAdvance call 2: " + calls.get(1));
        System.out.println("CK RJdkPhaser onAdvance calls=" + calls
                + " terminatedPhase=0x" + Integer.toHexString(p.getPhase()));
    }

    /** forceTermination() sets the sign bit and preserves everything else. */
    static void forceTermination() {
        Phaser p = new Phaser(2);
        check(p.arrive() == 0, "one of two arrives");
        p.forceTermination();
        check(p.getPhase() == Integer.MIN_VALUE,
                "forceTermination in phase 0 must give 0x80000000, got "
                        + Integer.toHexString(p.getPhase()));
        // The party counts are NOT reset by forceTermination.
        state(p, Integer.MIN_VALUE, 2, 1, 1, true, "forceTermination preserves the counters");
        check(p.arrive() == Integer.MIN_VALUE, "arrive() on a force-terminated phaser");
        check(p.arriveAndAwaitAdvance() == Integer.MIN_VALUE,
                "arriveAndAwaitAdvance() must return immediately on a terminated phaser");
        check(p.register() == Integer.MIN_VALUE, "register() on a force-terminated phaser");
        state(p, Integer.MIN_VALUE, 2, 1, 1, true, "still inert after three calls");

        // The phase is preserved under the sign bit, not replaced by a sentinel.
        Phaser q = new Phaser(1);
        q.arrive();
        q.arrive();
        state(q, 2, 1, 0, 1, false, "single-party phaser at phase 2");
        q.forceTermination();
        check(q.getPhase() == (Integer.MIN_VALUE | 2),
                "forceTermination must keep phase 2 under the sign bit, got 0x"
                        + Integer.toHexString(q.getPhase()));
        System.out.println("CK RJdkPhaser forceTermination phase=0x"
                + Integer.toHexString(q.getPhase()));
    }

    /** Tiering: a child registers exactly one party with its parent. */
    static void tiered() {
        Phaser parent = new Phaser(1);
        Phaser child = new Phaser(parent, 2);
        Phaser empty = new Phaser(parent, 0);

        check(child.getParent() == parent, "getParent()");
        check(child.getRoot() == parent, "getRoot() of a one-deep child is its parent");
        check(parent.getRoot() == parent, "a root phaser is its own root");
        check(empty.getParent() == parent, "the empty child still has a parent");

        // The 2-party child counts as ONE party at the parent; the 0-party
        // child counts as none at all.
        state(parent, 0, 2, 0, 2, false, "parent sees itself plus one child party");
        state(child, 0, 2, 0, 2, false, "child keeps its own two parties");
        state(empty, 0, 0, 0, 0, false, "a 0-party child registers nothing upward");

        check(child.arrive() == 0, "child arrive #1");
        state(parent, 0, 2, 0, 2, false, "a partial child arrival does not reach the parent");
        state(child, 0, 2, 1, 1, false, "child, one arrived");
        check(child.arrive() == 0, "child arrive #2");
        // The child completing its phase is the child's single arrival at the
        // parent -- but the parent has not advanced, so the child has not either.
        state(parent, 0, 2, 1, 1, false, "the completed child arrives once at the parent");
        state(child, 0, 2, 2, 0, false, "the child waits on the root's phase");

        check(parent.arrive() == 0, "the parent's own party arrives");
        state(parent, 1, 2, 0, 2, false, "the root advances");
        state(child, 1, 2, 0, 2, false, "and the child reads the root's phase");

        check(empty.register() == 1, "register() on the empty child returns the root phase");
        state(parent, 1, 3, 0, 3, false, "the empty child now holds a party at the parent");
        state(empty, 1, 1, 0, 1, false, "the empty child has one party of its own");
        System.out.println("CK RJdkPhaser tiered parentReg=" + parent.getRegisteredParties()
                + " childPhase=" + child.getPhase());
    }

    /** toString() reports the real state word, not a fabricated one. */
    static void toStringShape() {
        Phaser p = new Phaser(4);
        p.arrive();
        String s = p.toString();
        check(s.startsWith("java.util.concurrent.Phaser@"),
                "toString must name the real class: " + s);
        int lb = s.indexOf('[');
        check(lb > 0, "toString must carry a bracketed state: " + s);
        String tail = s.substring(lb);
        check(tail.equals("[phase = 0 parties = 4 arrived = 1]"), "toString tail: " + tail);
        // Printed WITHOUT the identity hash, so the line is comparable to HotSpot's.
        System.out.println("CK RJdkPhaser toString=" + tail);
    }

    public static void main(String[] args) throws Exception {
        countersAndArrive();
        registration();
        deregister();
        unregisteredArrival();
        awaitAdvanceThreads();
        awaitAdvancePastPhase();
        onAdvanceTermination();
        forceTermination();
        tiered();
        toStringShape();
        System.out.println("CK RJdkPhaser checks=" + checks);
        System.out.println("PASS RJdkPhaser (" + checks + " checks)");
    }
}
