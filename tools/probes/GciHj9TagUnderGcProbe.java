// gci wave i9, lane hj: the C JVMTI tag functions (`SetTag`, `GetTag`,
// `GetObjectsWithTags`) while another thread asks for full collections back
// to back and a third churns the young generation
// (docs/known-issues/gc/gci-i8hd-jvmti-tag-functions-and-oome-dump-outside-a-pause-20261013.md,
// items 1 and 2).
//
// CratonVM used to decode the object handle under a JNI entry, drop the entry
// (the thread back in native, GC-safe), then look the OLD address up in the
// tag table: a collection completed in between made `GetTag` answer 0 (or
// another object's tag) and `SetTag` tag the vacated address.
// `GetObjectsWithTags` resolved its weak globals in native and minted the
// local handles after taking the entry. Callers that are GC-safe in their C
// code: a Java thread in a JNI native (the JNI transition package is ON by
// default) and an agent thread (`RunAgentThread`). Both run here.
//
// Two populations of `Node`s (fresh, so young at first): MAIN's, worked by
// the main thread through JNI natives, and AGENT's, worked by an agent thread
// in C. Every round, per population: clear every tag, tag object i with
// base+i, read every tag back, and ask GetObjectsWithTags for the whole tag
// range, checking each returned object's `value` field against its tag.
//
// Needs a CratonVM built with the vm crate's `experimental-debug` feature (the
// C jvmtiEnv) and the shim tools/probes/jni/GciHj9TagUnderGcProbe.c, loaded as
// an agent AND with System.load (args[0]) for the natives:
//
//   cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciHj9TagUnderGcProbe.c /Fegcihj9tag.dll
//   java     -XX:+UseSerialGC -Xmx64m -agentpath:<dll> -cp <classes> GciHj9TagUnderGcProbe <dll> 100
//   cratonvm -Xmx64m -agentpath:<dll> -cp <classes> GciHj9TagUnderGcProbe <dll> 100
//
// Expected stdout (HotSpot 25; deterministic):
//
//   init 0
//   main rounds 100 setErr 0 badGet 0 badQuery 0 firstBad 0
//   agent start 0 done 1 rounds 100 setErr 0 badGet 0 badQuery 0 firstBad 0
//   values 2000 sum 999000
//
// stderr (diagnostics only): `[hj9t] gcs=<System.gc calls> main_ms=<n> agent_ms=<n>`.
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

public class GciHj9TagUnderGcProbe {
    public static class Node {
        public int value;

        public Node(int value) {
            this.value = value;
        }
    }

    static final int N = 1000;

    /** Caches `Node.value`'s field id; 0, or -1 without the agent. */
    static native int init(Class<?> nodeClass);

    /** `SetTag(objs[i], 0)` for every i: the number of errors. */
    static native int clearAll(Object[] objs);

    /** `SetTag(objs[i], base + i)`: the number of errors. */
    static native int tagAll(Object[] objs, long base);

    /** `GetTag(objs[i])` against `base + i`: the number of mismatches or errors. */
    static native int checkAll(Object[] objs, long base);

    /**
     * `GetObjectsWithTags` for `base .. base+n-1`: `bad * 1_000_000 + found`, a
     * result is bad when its `value` is not its tag minus `base` or `GetTag`
     * disagrees; `-error` on an error.
     */
    static native long query(long base, int n);

    /** `RunAgentThread(t, ...)`: the agent thread runs `rounds` rounds over `objs`. */
    static native int startAgent(Thread t, Object[] objs, int rounds, long baseStart);

    /** The agent's counters: done, rounds, setErr, badGet, badQuery, firstBad. */
    static native long agentStat(int which);

    static final Object[] RING = new Object[256];

    static Object[] population(int offset) {
        Object[] objs = new Object[N];
        for (int i = 0; i < N; i++) {
            objs[i] = new Node(offset + i);
        }
        return objs;
    }

    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 100;
        System.out.println("init " + init(Node.class));
        Object[] mine = population(0);
        Object[] theirs = population(0);

        AtomicBoolean stop = new AtomicBoolean();
        AtomicLong gcs = new AtomicLong();
        Thread gc = new Thread(() -> {
            while (!stop.get()) {
                System.gc();
                gcs.incrementAndGet();
            }
        }, "gcloop");
        Thread alloc = new Thread(() -> {
            int k = 0;
            while (!stop.get()) {
                RING[k & 255] = new byte[512];
                k++;
            }
        }, "alloc");
        gc.setDaemon(true);
        alloc.setDaemon(true);
        gc.start();
        alloc.start();

        // The agent's tags live in their own range: base 1 << 40 plus 10 000 per round.
        Thread agent = new Thread("hj9-agent");
        agent.setDaemon(true);
        long agentStart = System.nanoTime();
        int started = startAgent(agent, theirs, rounds, 1L << 40);

        long mainStart = System.nanoTime();
        int setErr = 0;
        int badGet = 0;
        int badQuery = 0;
        long firstBad = 0;
        for (int r = 1; r <= rounds; r++) {
            long base = 10_000L * r;
            int e = clearAll(mine) + tagAll(mine, base);
            int g = checkAll(mine, base);
            long q = query(base, N);
            long found = q < 0 ? q : q % 1_000_000;
            long bad = q < 0 ? 0 : q / 1_000_000;
            setErr += e;
            badGet += g;
            if (q < 0 || found != N || bad != 0) {
                badQuery++;
            }
            if (firstBad == 0 && (e != 0 || g != 0 || q < 0 || found != N || bad != 0)) {
                firstBad = r;
            }
        }
        long mainMs = (System.nanoTime() - mainStart) / 1_000_000;
        System.out.println("main rounds " + rounds + " setErr " + setErr + " badGet " + badGet
                + " badQuery " + badQuery + " firstBad " + firstBad);

        long deadline = System.nanoTime() + 240_000_000_000L;
        while (started == 0 && agentStat(0) == 0 && System.nanoTime() < deadline) {
            Thread.sleep(10);
        }
        long agentMs = (System.nanoTime() - agentStart) / 1_000_000;
        stop.set(true);
        gc.join();
        alloc.join();
        System.out.println("agent start " + started + " done " + agentStat(0) + " rounds " + agentStat(1)
                + " setErr " + agentStat(2) + " badGet " + agentStat(3) + " badQuery " + agentStat(4)
                + " firstBad " + agentStat(5));
        long sum = 0;
        int len = 0;
        for (Object[] objs : new Object[][] {mine, theirs}) {
            for (Object o : objs) {
                sum += ((Node) o).value;
                len++;
            }
        }
        System.out.println("values " + len + " sum " + sum);
        System.err.println("[hj9t] gcs=" + gcs.get() + " main_ms=" + mainMs + " agent_ms=" + agentMs);
    }
}
