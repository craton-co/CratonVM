import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.concurrent.TimeUnit;

/**
 * Does a native registered on an ABSTRACT `java.lang.Process` method intercept
 * a `Process` the APPLICATION wrote?
 *
 * `l5-native-io-bridge-residuals.md` left this as its fifth open item:
 *
 * > The abstract ones intercept **every** implementor, including a user
 * > subclass of `Process` — the same hazard `register_interface_natives`
 * > carries.
 *
 * The census confirms the surface: `register_process_natives` registers all six
 * of `java.lang.Process`'s abstract methods (`waitFor()I`, `exitValue`,
 * `destroy`, `getInputStream`, `getErrorStream`, `getOutputStream`) as
 * `Bridge`, so every subclass inherits a VM native in front of its own body.
 * Surface is not behaviour, and this is the question the surface cannot answer.
 *
 * The shape is `UserImplementorInterceptProbe`'s, which settled the same
 * question for `Map`/`Collection`: subclass the intercepted type, answer values
 * no real `Process` ever would, call through the `Process` supertype, and print
 * every observable as `key=value` so the run diffs byte-for-byte against real
 * HotSpot. Agreement means dispatch reaches the application's bytecode and the
 * interception surface is inert; a divergence is a native answering for a class
 * it has never seen.
 *
 * `Process` is a better test than `Map` was, for two reasons: it is an abstract
 * CLASS rather than an interface, so it exercises the superclass-walk arm of
 * dispatch rather than the interface arm; and the VM's own natives here are
 * backed by a real subprocess implementation, so a leak would return a plausible
 * value rather than an obvious sentinel.
 *
 *   javac -d out probes/UserProcessInterceptProbe.java
 *   java  -cp out UserProcessInterceptProbe        # HotSpot control
 *   cratonvm --real-jdk --java-home $JDK -cp out UserProcessInterceptProbe
 */
public class UserProcessInterceptProbe {

    /**
     * Answers deliberately unlike any real `Process`: every value is traceable
     * to this class and could not have come from a spawned subprocess.
     */
    static final class LoudProcess extends Process {
        int waitFors, exitValues, destroys, ins, errs, outs, alives;

        @Override public int waitFor() { waitFors++; return 4242; }
        @Override public int exitValue() { exitValues++; return 777; }
        @Override public void destroy() { destroys++; }

        @Override public InputStream getInputStream() {
            ins++;
            return new ByteArrayInputStream("stdout-from-user".getBytes());
        }

        @Override public InputStream getErrorStream() {
            errs++;
            return new ByteArrayInputStream("stderr-from-user".getBytes());
        }

        @Override public OutputStream getOutputStream() {
            outs++;
            return new ByteArrayOutputStream();
        }

        /**
         * Concrete on `Process` (bytecode, not abstract) and ALSO registered as
         * a native. Overriding it asks the §1.4 question next to the §1.5 one:
         * a native shadowing concrete bytecode must still lose to an override.
         */
        @Override public boolean isAlive() { alives++; return true; }
    }

    /**
     * A `Process` that has NOT exited, reported the way the JDK specifies:
     * `exitValue()` throws `IllegalThreadStateException`.
     */
    static final class StillRunningProcess extends Process {
        int exitValues, waitFors;

        @Override public int exitValue() {
            exitValues++;
            throw new IllegalThreadStateException("not exited");
        }

        @Override public int waitFor() { waitFors++; return 0; }
        @Override public void destroy() { }
        @Override public InputStream getInputStream() {
            return new ByteArrayInputStream(new byte[0]);
        }
        @Override public InputStream getErrorStream() {
            return new ByteArrayInputStream(new byte[0]);
        }
        @Override public OutputStream getOutputStream() {
            return new ByteArrayOutputStream();
        }
    }

    /**
     * Overrides ONLY what `Process` leaves abstract — exactly what a real
     * application subclass looks like. Every concrete method is inherited, so
     * each one is a place a native can win against `Process`'s own bytecode.
     */
    static final class PlainProcess extends Process {
        int exitValues, waitFors, destroys;

        @Override public int exitValue() {
            exitValues++;
            throw new IllegalThreadStateException("not exited");
        }

        @Override public int waitFor() { waitFors++; return 3; }
        @Override public void destroy() { destroys++; }
        @Override public InputStream getInputStream() {
            return new ByteArrayInputStream(new byte[0]);
        }
        @Override public InputStream getErrorStream() {
            return new ByteArrayInputStream(new byte[0]);
        }
        @Override public OutputStream getOutputStream() {
            return new ByteArrayOutputStream();
        }
    }

    /** Print the exception type instead of dying, so one bad rung is one line. */
    private static String safe(java.util.function.Supplier<String> f) {
        try {
            return f.get();
        } catch (Throwable t) {
            return "EXC:" + t.getClass().getName();
        }
    }

    private static String read(InputStream in) {
        try {
            return new String(in.readAllBytes());
        } catch (Exception e) {
            return "EXC:" + e.getClass().getName();
        }
    }

    public static void main(String[] args) throws Exception {
        LoudProcess lp = new LoudProcess();
        // Through the SUPERTYPE, which is the shape the natives are registered
        // on. A `LoudProcess`-typed call could bind directly and prove nothing.
        Process p = lp;

        System.out.println("waitFor=" + p.waitFor());
        System.out.println("exitValue=" + p.exitValue());
        System.out.println("stdout=" + read(p.getInputStream()));
        System.out.println("stderr=" + read(p.getErrorStream()));
        System.out.println("outputStreamClass=" + p.getOutputStream().getClass().getName());
        System.out.println("isAlive=" + p.isAlive());
        p.destroy();

        // Default methods on `Process` that the VM also registers, and that are
        // specified to be built out of the abstract ones above. If a native
        // answers these, the counters below will not have moved.
        System.out.println("waitForTimeout=" + p.waitFor(1, TimeUnit.NANOSECONDS));

        // The discriminating case. `Process.waitFor(long, TimeUnit)` is concrete
        // bytecode specified in terms of `exitValue()`: it polls, and treats
        // `IllegalThreadStateException` as "not exited yet". So a process whose
        // `exitValue` throws MUST make `waitFor(0, ..)` answer `false`.
        //
        // A native standing in front of that bytecode cannot know this without
        // calling the subclass, and the previous rung showed it does not call it
        // — that rung's `true` was right by accident, because a `Process` that
        // returns an exit value has in fact exited. This one separates the two.
        StillRunningProcess sr = new StillRunningProcess();
        Process rp = sr;
        boolean exitedWithinZero;
        try {
            exitedWithinZero = rp.waitFor(0, TimeUnit.NANOSECONDS);
        } catch (IllegalThreadStateException e) {
            // Leaking the poll's own control-flow exception to the caller is a
            // third possible wrong answer, and worth naming distinctly.
            exitedWithinZero = false;
            System.out.println("stillRunning.leakedITSE=true");
        }
        System.out.println("stillRunning.waitForZero=" + exitedWithinZero
                + " (specified: false)");
        System.out.println("stillRunning.count.exitValue=" + sr.exitValues
                + " (specified: >0 — waitFor is built out of it)");
        System.out.println("stillRunning.VERDICT="
                + (!exitedWithinZero && sr.exitValues > 0
                        ? "bytecode-polled-the-subclass"
                        : "A-NATIVE-ANSWERED-WITHOUT-ASKING-THE-SUBCLASS"));

        // --- the CONCRETE surface, none of it overridden by LoudProcess ------
        //
        // `java.lang.Process` declares these with real bytecode built out of the
        // abstract methods above, and `register_process_natives` registers each
        // of them too. A subclass that does not override them is where the
        // native and the bytecode actually compete, and §1.4 says the bytecode
        // wins. Printed as `key=value` so HotSpot decides what each should say.
        PlainProcess plain = new PlainProcess();
        Process cp = plain;
        System.out.println("concrete.isAlive=" + safe(() -> String.valueOf(cp.isAlive())));
        System.out.println("concrete.pid=" + safe(() -> String.valueOf(cp.pid())));
        System.out.println("concrete.toHandleNull="
                + safe(() -> String.valueOf(cp.toHandle() == null)));
        System.out.println("concrete.destroyForciblySelf="
                + safe(() -> String.valueOf(cp.destroyForcibly() == cp)));

        // Snapshot the counters HERE, before `onExit`.
        //
        // `Process.onExit()`'s default is
        // `CompletableFuture.supplyAsync(this::waitForInternal)` — it calls the
        // subclass on a POOL thread, so whether it has bumped `exitValues` by
        // the time this method prints is a race with no bound. Observed once on
        // real HotSpot as `2` against `1` on eleven other runs across both VMs,
        // which is exactly rare enough to be misread as a regression in an
        // unrelated change. The rungs above are all synchronous.
        int cExit = plain.exitValues;
        int cWait = plain.waitFors;
        int cDestroy = plain.destroys;

        System.out.println("concrete.onExitNull="
                + safe(() -> String.valueOf(cp.onExit() == null)));

        // After the synchronous calls above, a correct run has reached the
        // subclass's own abstract-method overrides, because that is what the
        // bytecode is made of. Zeroes here mean a native answered without
        // asking.
        System.out.println("concrete.count.exitValue=" + cExit);
        System.out.println("concrete.count.waitFor=" + cWait);
        System.out.println("concrete.count.destroy=" + cDestroy);

        // The counters are the actual assertion: every call above must have
        // landed in this class.
        //
        // ORDERING REQUIREMENT: every rung above this point is synchronous. Do
        // not append a rung that schedules work on another thread (`onExit`,
        // anything building a `CompletableFuture`) before these prints — see the
        // snapshot in the concrete section for what that costs.
        System.out.println("count.waitFor=" + lp.waitFors);
        System.out.println("count.exitValue=" + lp.exitValues);
        System.out.println("count.destroy=" + lp.destroys);
        System.out.println("count.getInputStream=" + lp.ins);
        System.out.println("count.getErrorStream=" + lp.errs);
        System.out.println("count.getOutputStream=" + lp.outs);
        System.out.println("count.isAlive=" + lp.alives);

        boolean allReached = lp.waitFors > 0 && lp.exitValues > 0 && lp.destroys > 0
                && lp.ins > 0 && lp.errs > 0 && lp.outs > 0 && lp.alives > 0;
        System.out.println("VERDICT=" + (allReached
                ? "application-bytecode-answered-every-call"
                : "A-NATIVE-INTERCEPTED-A-USER-SUBCLASS"));
    }
}
