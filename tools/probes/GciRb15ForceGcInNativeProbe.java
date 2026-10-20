// gci wave i15, lane rb: JVMTI `ForceGarbageCollection` called from C code that
// runs IN NATIVE -- a JNI native method's (the JNI transition package,
// `CRATONVM_JNI_NATIVE_TRANSITIONS`, is ON by default) and a `RunAgentThread`
// start function's (in native since gci i15/hx) -- while another thread churns
// the young generation
// (docs/known-issues/gc/gci-i15rb-vm-default-path-changes-review-findings-20261014.md,
// finding 1).
//
// CratonVM's `force_garbage_collection` (vm/src/jvmti/native_env/heap.rs) built
// its context without the JNI entry, so the collection, and the forced
// finalizer drain behind it (Java, run inline on the calling thread), ran with
// the caller still flagged in native: excluded from every other thread's pause,
// with only its stale deposit as its roots. `Fin.finalize` allocates and keeps
// young references in its locals across those allocations, then re-checks
// them; a reference the churn thread's collection moved under it shows as
// `bad` > 0 (or a crash).
//
// Needs a CratonVM built with the vm crate's `experimental-debug` feature (the
// C jvmtiEnv) and the shim tools/probes/jni/GciRb15ForceGcInNativeProbe.c,
// loaded as an agent AND with System.load (args[0]) for the natives:
//
//   cl /LD /O1 /I"%JDK%\include" /I"%JDK%\include\win32" GciRb15ForceGcInNativeProbe.c /Fegcirb15fgc.dll
//   java     -XX:+UseSerialGC -Xmx64m -agentpath:<dll> -cp <classes> GciRb15ForceGcInNativeProbe <dll> 50
//   cratonvm -Xmx64m -agentpath:<dll> -cp <classes> GciRb15ForceGcInNativeProbe <dll> 50
//
// Expected stdout (HotSpot 25; deterministic):
//
//   init 0
//   main rounds 50 rc 0
//   agent start 0 done 1 count 50 err 0
//   bad 0
//
// stderr (diagnostics only): `[rb15f] finalized=<n> main_ms=<n> agent_ms=<n>`.
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

public class GciRb15ForceGcInNativeProbe {
    static final AtomicLong FINALIZED = new AtomicLong();
    static final AtomicLong BAD = new AtomicLong();
    static final Object[] RING = new Object[256];
    static volatile Object sink;

    static final class Fin {
        final int id;
        final int[] payload;

        Fin(int id) {
            this.id = id;
            this.payload = new int[64];
            for (int i = 0; i < 64; i++) {
                payload[i] = id + i;
            }
        }

        @SuppressWarnings({"deprecation", "removal"})
        @Override
        protected void finalize() {
            int[] copy = payload.clone();
            StringBuilder sb = new StringBuilder();
            for (int k = 0; k < 64; k++) {
                sb.append(copy[k]).append(',');
                sink = new byte[256];
            }
            String s = sb.toString();
            int k = 0;
            boolean ok = true;
            for (String part : s.split(",")) {
                if (Integer.parseInt(part) != id + k) {
                    ok = false;
                }
                k++;
            }
            for (int j = 0; j < 64; j++) {
                if (copy[j] != id + j || payload[j] != id + j) {
                    ok = false;
                }
            }
            if (!ok || k != 64) {
                BAD.incrementAndGet();
            }
            FINALIZED.incrementAndGet();
        }
    }

    /** 0 with the agent's jvmtiEnv, -1 without. */
    static native int init();

    /** `ForceGarbageCollection` n times from this JNI native's C code: 0 or -error. */
    static native int forceGc(int n);

    /** `RunAgentThread(t, ...)`: the agent's C code forces n collections. */
    static native int startAgent(Thread t, int n);

    /** The agent's counters: done, count, first error. */
    static native long agentStat(int which);

    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 50;
        System.out.println("init " + init());

        AtomicBoolean stop = new AtomicBoolean();
        Thread alloc = new Thread(() -> {
            int k = 0;
            while (!stop.get()) {
                RING[k & 255] = new byte[512];
                k++;
            }
        }, "alloc");
        alloc.setDaemon(true);
        alloc.start();

        Thread agent = new Thread("rb15-agent");
        agent.setDaemon(true);
        long agentStart = System.nanoTime();
        int started = startAgent(agent, rounds);

        long mainStart = System.nanoTime();
        int rc = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 200; i++) {
                sink = new Fin(r * 1000 + i);
            }
            sink = null;
            int e = forceGc(1);
            if (e != 0 && rc == 0) {
                rc = e;
            }
        }
        long mainMs = (System.nanoTime() - mainStart) / 1_000_000;

        long deadline = System.nanoTime() + 240_000_000_000L;
        while (started == 0 && agentStat(0) == 0 && System.nanoTime() < deadline) {
            Thread.sleep(10);
        }
        long agentMs = (System.nanoTime() - agentStart) / 1_000_000;
        stop.set(true);
        alloc.join();
        System.out.println("main rounds " + rounds + " rc " + rc);
        System.out.println("agent start " + started + " done " + agentStat(0) + " count " + agentStat(1)
                + " err " + agentStat(2));
        System.out.println("bad " + BAD.get());
        System.err.println("[rb15f] finalized=" + FINALIZED.get() + " main_ms=" + mainMs
                + " agent_ms=" + agentMs);
    }
}
