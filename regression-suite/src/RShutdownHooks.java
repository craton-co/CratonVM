import java.io.FileDescriptor;
import java.io.FileOutputStream;

/**
 * Regression: `Runtime.addShutdownHook` registers the hook and then NOTHING
 * EVER RUNS IT.
 *
 * `native-builtins/src/lang_system.rs` roots each hook in a `SHUTDOWN_HOOKS`
 * vector — correctly, so the hook and everything it references stay alive —
 * and the only reader of that vector is `removeShutdownHook`. It is a
 * write-only list. Meanwhile `System.exit` / `Runtime.exit` /
 * `Shutdown.halt0` all go straight to `std::process::exit`, and the launcher's
 * post-`main` path exits without consulting it either. In `--real-jdk` mode the
 * JDK's own machinery is present and would work, but both ends of it are
 * intercepted: `Runtime.addShutdownHook` is a Java method with a `Code`
 * attribute (`--dump-native-registry`: `acc_native:false, has_code:true`) over
 * which CratonVM registers a native that owns the slot, so
 * `ApplicationShutdownHooks.hooks` is never populated; and `System.exit` is
 * intercepted too, so `Shutdown.exit` -> `runHooks()` is never called.
 *
 * Why this vector rather than a doc line: the gap has already produced
 * MEASUREMENT ERROR three times over. A corpus harness key included a
 * `completed=` marker printed from a shutdown hook; the marker never appeared;
 * and three separate lanes each read that as a sweeping cross-VM DIVERGE
 * verdict against the VM. One gap wore twelve, nine and thirty-six hats.
 *
 * THE LOAD-BEARING DESIGN DECISION: a hook that RUNS but whose output is
 * swallowed is indistinguishable from a hook that never ran, and would have
 * produced the same wrong verdict. So the hook writes on three channels —
 * `System.out`, `System.err`, and a raw `FileOutputStream(FileDescriptor.out)`
 * that bypasses `System.out` entirely — and reports the result of all three on
 * a `CK` line emitted over the RAW channel, the one a dead `System.out` cannot
 * eat. `run.sh`'s `extract()` keeps only `PASS `/`CK ` lines, so anything on
 * any other prefix is deleted before the cross-VM diff sees it (W7-60): a hook
 * whose evidence went to an unprefixed line would reduce this vector to "did
 * main reach its last statement", which is the shape W7-51 exists to catch.
 *
 * `main` returns normally — the path `run.sh` actually exercises. The other
 * four exit paths (System.exit from main, System.exit from a non-main thread,
 * an uncaught exception, and a non-daemon thread outliving main) are separate
 * code in this VM and are covered by the ShutdownProbe matrix recorded in
 * `docs/known-issues/jdk-only/W7-92-shutdown-hooks-never-run.md`; they are not
 * folded in here because a vector that terminates the process cannot also
 * print its own PASS line.
 *
 * Measured on HotSpot 25.0.3+9 (Microsoft build 25.0.3+9-LTS), Windows, rc=0.
 * The RAW output and the output after `run.sh`'s `extract()` filter are now the
 * SAME five lines — that is the contract, and it is what keeps guard G1 quiet:
 *   CK RShutdownHooks pre removed=true removedAgain=false dupIAE=true
 *   PASS RShutdownHooks (4 checks)
 *   CK RShutdownHooks hookErr ran=true
 *   CK RShutdownHooks hookOut ran=true
 *   CK RShutdownHooks hookFd1 ran=true ownThread=true err=ok out=ok
 * PREDICTED on CratonVM: the first two lines match, the last three are ABSENT.
 *
 * Until 2026-08-13 the third line read `hook-stderr` and the second read
 * `PASS RShutdownHooks checks=4`. Both were harness-invisible: extract() drops
 * any line not prefixed `PASS `/`CK `, so the oracle printed evidence the suite
 * could not see (G1), and `harness_check_count` parses only
 * `PASS <Class> (N checks)` / `CK <Class> checks=N`, so the published count was
 * unreadable (G3). Both guards fired on HOTSPOT, which is what made this
 * vector's CratonVM result uninterpretable in either direction; neither was a
 * fact about either VM.
 *
 * Mutation-checked (see the record): dropping the `addShutdownHook(live)` call
 * deletes both hook lines; letting a REMOVED hook run adds a line; and a hook
 * whose System.out is dead loses `hookOut` while `hookFd1` survives carrying
 * `out=LOST`. Three distinct filtered outputs for three distinct VM states.
 */
public class RShutdownHooks {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    /** Written straight to fd 1, bypassing System.out, and flushed. */
    static boolean rawOk(String s) {
        try {
            FileOutputStream fd1 = new FileOutputStream(FileDescriptor.out);
            fd1.write((s + "\n").getBytes());
            fd1.flush();
            return true;
        } catch (Throwable t) {
            return false;
        }
    }

    static Thread hook() {
        Thread t = new Thread(() -> {
            // `ownThread` is what proves the hook was STARTED as a thread
            // rather than having its run() called inline on the exiting
            // thread. A VM that fixes this by calling run() inline answers
            // ownThread=false and this line still differs from HotSpot's,
            // which is deliberate: inline execution is not equivalent for a
            // hook that waits on another hook, and the diff should say so.
            boolean own = "cratonvm-hook-A".equals(Thread.currentThread().getName());
            boolean errOk;
            try {
                // CK-prefixed on PURPOSE. This line used to read `hook-stderr`,
                // which extract() DELETES — so the oracle printed one line the
                // suite cannot see and guard G1 fired on HotSpot every run,
                // making this vector's redness uninterpretable in either
                // direction. The channel still has to be exercised (that is what
                // `errOk` measures), and a probe whose output is deleted proves
                // nothing about the channel, so the probe's text carries the
                // prefix. It also strengthens the vector: `err=ok` on the fd1
                // line says the write did not throw, while THIS line landing
                // says the bytes actually arrived — a VM with a silently dead
                // System.err answers err=ok and still loses this line.
                //
                // Ordering is deterministic and already load-bearing here: err,
                // out and fd1 are written and flushed in sequence by one thread
                // onto one merged stream (run.sh captures 2>&1), which is why
                // hookOut/hookFd1 could already be diffed as an ordered pair.
                System.err.println("CK RShutdownHooks hookErr ran=true");
                System.err.flush();
                errOk = true;
            } catch (Throwable e) {
                errOk = false;
            }
            boolean outOk;
            try {
                System.out.println("CK RShutdownHooks hookOut ran=true");
                System.out.flush();
                outOk = true;
            } catch (Throwable e) {
                outOk = false;
            }
            // THE DISCRIMINATING LINE, and it is deliberately NOT on
            // System.out. If the hook runs on a VM whose System.out is dead or
            // unflushed at shutdown, the `hookOut` line above is lost and this
            // one still lands — "ran, output lost" and "never ran" stop being
            // the same observation. Putting both lines on System.out would
            // reintroduce exactly the ambiguity this vector exists to remove.
            rawOk("CK RShutdownHooks hookFd1 ran=true ownThread=" + own
                    + " err=" + (errOk ? "ok" : "LOST")
                    + " out=" + (outOk ? "ok" : "LOST"));
        });
        t.setName("cratonvm-hook-A");
        return t;
    }

    public static void main(String[] args) throws Exception {
        Runtime rt = Runtime.getRuntime();

        // A hook that is registered and then removed must NOT run. Without
        // this negative arm, a VM that ran every Thread it had ever seen
        // would satisfy the positive arm.
        Thread doomed = new Thread(() ->
                System.out.println("CK RShutdownHooks REMOVED-HOOK-RAN (must not appear)"));
        doomed.setName("cratonvm-hook-removed");
        rt.addShutdownHook(doomed);
        boolean removed = rt.removeShutdownHook(doomed);
        boolean removedAgain = rt.removeShutdownHook(doomed);
        check(removed, "removeShutdownHook must return true for a registered hook");
        check(!removedAgain, "removeShutdownHook must return false the second time");

        Thread live = hook();
        rt.addShutdownHook(live);

        // Re-registering a LIVE-or-pending hook is IllegalArgumentException.
        // (A thread that already ran to completion is NOT alive and IS
        // accepted — measured on HotSpot 25.0.3+9 — so that case is
        // deliberately not asserted here.)
        boolean dupIae = false;
        try {
            rt.addShutdownHook(live);
        } catch (IllegalArgumentException e) {
            dupIae = true;
        }
        check(dupIae, "re-adding an already-registered hook must throw IllegalArgumentException");

        // A hook must not be started at registration time.
        check(!live.isAlive(), "a registered hook must not be running yet");

        System.out.println("CK RShutdownHooks pre removed=" + removed
                + " removedAgain=" + removedAgain + " dupIAE=" + dupIae);
        // PARENTHESISED, and that is not cosmetic. harness_check_count parses
        // exactly two spellings — `PASS <Class> (N checks)` and
        // `CK <Class> checks=N` — and nothing else. This line said
        // `PASS RShutdownHooks checks=4`, which matches NEITHER, so the vector
        // published a count that the guard could not read and G3 fired on it
        // every run. A census of the suite found this file was the only one of
        // 77 counting vectors using that spelling; the other 76 are already
        // parenthesised.
        System.out.println("PASS RShutdownHooks (" + checks + " checks)");
        // main returns normally; the hook's CK line must follow.
    }
}
