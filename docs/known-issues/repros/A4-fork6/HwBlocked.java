// Deterministic repro for the A4 helper-window/blocked JIT-band root gap.
//
// hot() is warmed until JIT-compiled, then per trigger rep it builds a young
// String whose ONLY reference is hot()'s own JIT frame (a spill slot across
// the nap() call), blocks in Thread.sleep under that frame while a sibling
// thread hammers System.gc(), and finally verifies the String's content.
//
// Gap (pre-fix): the sleeper is barrier-excluded (blocked); its
// deposit_root_snapshot covers only interpreter frames, so the token in the
// JIT frame is invisible to the STW initiator and the sweep reclaims it —
// content check fails or the VM crashes on an all-zero header.
// Fixed: the post-barrier helper-window pass conservatively scans the blocked
// thread's stack band (it carries a JIT return address) and roots the token.
public class HwBlocked {
    static volatile boolean done = false;
    static volatile Object SINK;
    static volatile Object SINK2;

    // Block via park (NOT Thread.sleep): sleep in this VM is a raw pump loop
    // that stays COUNTED in the STW barrier (the GC just waits for it, and the
    // post-wake safepoint republishes the JIT chain), while park runs the real
    // blocked protocol — deposit_root_snapshot + enter_blocked — which is the
    // exact path with the JIT-band gap (same as ForkJoinTask.join).
    static void nap(int ms) {
        java.util.concurrent.locks.LockSupport.parkNanos(ms * 1_000_000L);
    }

    // No string concat (invokedynamic) and no try/catch in the hot method —
    // keep it JIT-compilable: arithmetic + array stores + new String(char[]).
    static String hot(int n, int rep, int sleepMs) {
        int acc = 0;
        for (int i = 0; i < n; i++) acc += i * 31 + rep;
        char[] cs = new char[8];
        int v = acc ^ rep;
        for (int i = 0; i < 8; i++) cs[i] = (char) ('a' + ((v >> (i * 4)) & 0xf));
        String token = new String(cs);
        if (sleepMs > 0) nap(sleepMs);
        return token;
    }

    static String expect(int n, int rep) {
        int acc = 0;
        for (int i = 0; i < n; i++) acc += i * 31 + rep;
        char[] cs = new char[8];
        int v = acc ^ rep;
        for (int i = 0; i < 8; i++) cs[i] = (char) ('a' + ((v >> (i * 4)) & 0xf));
        return new String(cs);
    }

    public static void main(String[] a) {
        int warm = a.length > 0 ? Integer.parseInt(a[0]) : 4000;
        int reps = a.length > 1 ? Integer.parseInt(a[1]) : 40;
        int sleepMs = a.length > 2 ? Integer.parseInt(a[2]) : 150;

        Thread gcThread = new Thread(() -> {
            while (!done) { System.gc(); nap(10); }
        });
        gcThread.setDaemon(true);
        gcThread.start();

        // Churn: constantly allocate token-shaped objects so a young block
        // freed by a wrongly-missed root is REUSED (content clobbered) while
        // the victim thread is still parked — a freed-but-untouched block
        // would otherwise still read back its old bytes and mask the bug.
        Thread churnThread = new Thread(() -> {
            char[] z = new char[8];
            java.util.Arrays.fill(z, 'z');
            while (!done) {
                for (int i = 0; i < 4096; i++) { SINK = new String(z); SINK2 = new char[8]; }
                nap(1);
            }
        });
        churnThread.setDaemon(true);
        churnThread.start();

        // Warm hot() past the JIT invocation threshold. Train the sleep branch
        // too (1 ms naps) so the compiled code doesn't deopt on the first
        // trigger rep when the branch flips.
        for (int i = 0; i < warm; i++) {
            if (hot(64, i, (i & 15) == 0 ? 1 : 0) == null) { System.out.println("warm null"); return; }
        }
        // Trigger: block under the compiled hot() frame while GCs storm.
        for (int rep = 0; rep < reps; rep++) {
            String got;
            try { got = hot(64, rep, sleepMs); }
            catch (Throwable t) { System.out.println("rep=" + rep + " THREW " + t); done = true; return; }
            String want = expect(64, rep);
            if (!want.equals(got)) {
                System.out.println("rep=" + rep + " FAIL got=" + got + " want=" + want);
                done = true;
                return;
            }
        }
        done = true;
        System.out.println("ALL-OK reps=" + reps);
    }
}
