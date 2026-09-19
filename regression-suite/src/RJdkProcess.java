import java.io.File;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.Optional;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;
import java.util.stream.Stream;

/**
 * JDK-only corpus: {@code ProcessHandle} -- current process, parent, info,
 * liveness, child process.
 *
 * "{@code ProcessHandle}" is a named P1 blocker: CratonVM's
 * {@code is_native_backed_jdk_stub} explicitly allows
 * {@code java/lang/ProcessHandle} and {@code java/lang/ProcessHandle$Info} to be
 * FABRICATED with a hand-written method table when boot bytes are unavailable.
 * Under {@code --jdk-only} the real classes must load and the platform leaves
 * must be bridges.
 *
 * Determinism: pids, command lines, users, start instants and cpu durations are
 * all host- and run-specific. NONE of them is printed -- only derived
 * predicates (present/absent, >0, alive/dead) and the child's exit code, which
 * this vector chooses itself.
 *
 * Determinism, part 2 -- process-TREE queries. {@code parent()},
 * {@code children()} and {@code descendants()} are documented snapshots of the
 * live OS process table. A process that has exited is not in that table any
 * more: on Windows it disappears as soon as it terminates, and on Unix as soon
 * as the JDK's process reaper waits on it. So those three queries may only ever
 * be asserted against a child that is still RUNNING and is guaranteed to stay
 * running for the whole observation window. This vector previously asserted
 * them against a child that exits immediately; real HotSpot 25 failed it
 * intermittently for exactly that reason. See
 * docs/known-issues/jdk-only/L10-rjdkprocess-vector-overassertion.md.
 */
public class RJdkProcess {
    static final long T = 60;
    /**
     * Bound on how long we will wait for a live child to show up in a
     * process-tree snapshot. Deliberately far below the sleeper's own 30s
     * lifetime, so a timeout means "the tree query is broken", never "the
     * child had already exited".
     */
    static final long TREE_WAIT_MS = 10_000;
    static int checks;

    /**
     * Every guard-suppressed assertion, by name.
     *
     * <p>This vector's own history is why it exists. Two {@code check(...)}
     * calls sit behind {@code if (info.command().isPresent())} and
     * {@code if (info.startInstant().isPresent())}. A JVM that reports those
     * fields empty does not fail here -- it simply never reaches them, prints
     * {@code PASS}, exits 0, and differs from HotSpot by nothing but the number
     * on the {@code checks=} line. That is strictly worse than a missing check,
     * because it reads as a passing one, and for four waves it read as one:
     * CratonVM printed {@code checks=51} against HotSpot's 53 and the only
     * signal was a two-digit diff a human had to notice.
     *
     * <p>The fix is not to assert those fields unconditionally -- they are
     * legally empty on a restricted platform, and {@code ProcessHandle.Info}'s
     * javadoc says so. It is to make the skip <em>say so</em>. {@link #skip}
     * counts, so {@link #checks} is INVARIANT across conforming JVMs and a
     * moved count is a hard {@link AssertionError}; and the reason is printed
     * on the {@code CK} line, so a legal-but-degraded answer shows up as a
     * textual diff against the oracle instead of an arithmetic one. Two
     * severities, neither of them silent.
     */
    static final List<String> skipped = new ArrayList<>();

    /**
     * How many {@code check(...)} + {@code skip(...)} calls a conforming JVM
     * makes, start to finish. Measured on HotSpot 25, where {@link #skipped} is
     * empty and every one of them is a real assertion.
     *
     * <p>Update this deliberately when adding or removing a check. It is the
     * ratchet: it is what turns "a check quietly stopped running" -- the defect
     * this vector was the first to exhibit -- into a failure.
     *
     * <p>One {@code check} in this file is dead on the oracle and is therefore
     * NOT counted: the {@code check(exit == null, "unreachable")} inside the
     * {@code onExit()} try-block, which HotSpot never reaches because
     * {@code onExit()} on the current process always throws. A VM that does not
     * throw runs it, the count becomes {@code EXPECTED_CHECKS + 1}, and this
     * constant catches that too.
     */
    static final int EXPECTED_CHECKS = 55;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /**
     * Record that a legally-optional guard was false, so the assertion behind
     * it did not run. Counts toward {@link #checks} deliberately -- see
     * {@link #skipped}.
     */
    static void skip(String why) {
        checks++;
        skipped.add(why);
    }

    static boolean windows() {
        return File.separatorChar == '\\';
    }

    /** A command that exits with code 3 and prints nothing at all. */
    static List<String> exitThree() {
        return windows()
                ? Arrays.asList("cmd.exe", "/c", "exit 3")
                : Arrays.asList("/bin/sh", "-c", "exit 3");
    }

    /** A command that sleeps long enough to be observed alive, then exits. */
    static List<String> sleepLong() {
        return windows()
                ? Arrays.asList("cmd.exe", "/c", "ping -n 30 127.0.0.1 > NUL")
                : Arrays.asList("/bin/sh", "-c", "sleep 30");
    }

    /**
     * Poll a process-tree snapshot until {@code pid} appears in it, bounded by
     * {@link #TREE_WAIT_MS}. A single sample is not a sound assertion: the
     * snapshot is taken at call time and a just-forked child is not obliged to
     * be in the very first one. The caller must keep the process alive for the
     * whole window, which makes a timeout a genuine failure of the query
     * rather than a lost race.
     */
    static boolean awaitInTree(long pid, boolean descendants) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(TREE_WAIT_MS);
        for (;;) {
            Stream<ProcessHandle> tree = descendants
                    ? ProcessHandle.current().descendants()
                    : ProcessHandle.current().children();
            if (tree.anyMatch(c -> c.pid() == pid)) {
                return true;
            }
            if (System.nanoTime() - deadline >= 0) {
                return false;
            }
            Thread.sleep(10);
        }
    }

    /**
     * Poll {@code p.descendants()} until it reports at least one process,
     * bounded by {@link #TREE_WAIT_MS}.
     *
     * <p>Bounded polling rather than a single sample, for the reason
     * {@link #awaitInTree} gives: a snapshot is taken at call time and a
     * just-forked grandchild is not obliged to be in the first one. The bound
     * sits far below the sleeper's own 30s lifetime, so a timeout means the
     * query is broken and never that the subtree had already gone.
     *
     * <p>The caller must first have established that the subject really does
     * have a descendant on this host -- otherwise a VM answering an empty
     * stream for the WRONG process is indistinguishable from a correct one,
     * which is the whole defect this exists to catch.
     */
    static boolean awaitOwnDescendant(Process p) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(TREE_WAIT_MS);
        for (;;) {
            if (p.descendants().findAny().isPresent()) {
                return true;
            }
            if (System.nanoTime() - deadline >= 0) {
                return false;
            }
            Thread.sleep(10);
        }
    }

    static void currentProcess() {
        ProcessHandle self = ProcessHandle.current();
        check(self != null, "ProcessHandle.current()");
        check(self.pid() > 0, "current pid must be positive");
        check(self.isAlive(), "the current process must be alive");
        check(self.equals(ProcessHandle.current()), "current() must be equal across calls");
        check(self.hashCode() == ProcessHandle.current().hashCode(), "current() hashCode");
        check(ProcessHandle.of(self.pid()).isPresent(), "of(pid) must find the current process");
        check(ProcessHandle.of(self.pid()).get().equals(self), "of(pid) identity");

        // Destroying yourself is refused.
        boolean threw = false;
        try {
            self.destroy();
        } catch (IllegalStateException expected) {
            threw = true;
        }
        check(threw, "destroying the current process must raise IllegalStateException");

        // parent() is an Optional -- whether a parent is visible is
        // host-specific (a reaped or out-of-scope parent legitimately gives
        // empty), so only the call contract is asserted, never the value.
        Optional<ProcessHandle> parent = self.parent();
        check(parent != null, "parent() must never return null");

        // Info: every field is an Optional and every one of them is allowed to
        // be empty on a restricted platform. Assert the SHAPE, print nothing.
        ProcessHandle.Info info = self.info();
        check(info != null, "info() must never return null");
        check(info.command() != null, "info().command()");
        check(info.commandLine() != null, "info().commandLine()");
        check(info.arguments() != null, "info().arguments()");
        check(info.startInstant() != null, "info().startInstant()");
        check(info.totalCpuDuration() != null, "info().totalCpuDuration()");
        check(info.user() != null, "info().user()");
        check(info.toString() != null, "info().toString()");
        // If the command IS reported it must name an executable, not be blank.
        // The `else` is not decoration: HotSpot 25 reports both of these on
        // every platform this suite runs on, so an empty one is the two-check
        // silent drop this vector's `skipped` list exists to name.
        if (info.command().isPresent()) {
            check(!info.command().get().trim().isEmpty(), "reported command must not be blank");
        } else {
            skip("info.command() empty -- 'reported command must not be blank' did not run");
        }
        if (info.startInstant().isPresent()) {
            check(info.startInstant().get().toEpochMilli() > 0, "start instant must be positive");
        } else {
            skip("info.startInstant() empty -- 'start instant must be positive' did not run");
        }

        // onExit() on the CURRENT process is explicitly disallowed.
        threw = false;
        try {
            CompletableFuture<ProcessHandle> exit = self.onExit();
            check(exit == null, "unreachable");
        } catch (IllegalStateException expected) {
            threw = true;
        }
        check(threw, "onExit() on the current process must raise IllegalStateException");

        // allProcesses() may be restricted, but must return a usable stream.
        long visible = ProcessHandle.allProcesses().limit(4).count();
        check(visible >= 0, "allProcesses must be enumerable");
        check(ProcessHandle.of(Long.MAX_VALUE).isEmpty(),
                "an impossible pid must not resolve");
        System.out.println("CK RJdkProcess self alive=" + self.isAlive()
                + " pidPositive=" + (self.pid() > 0)
                + " infoNonNull=true parentOptionalNonNull=" + (parent != null));
    }

    static void childProcess() throws Exception {
        // ------------------------------------------------------------------
        // Part 1: process-TREE relationships, asserted against a LIVE child.
        //
        // parent()/children()/descendants() read the live OS process table, so
        // the subject has to still be in it. The sleeper below runs for 30s;
        // every query here happens inside that window and is polled with a 10s
        // bound, so nothing depends on scheduling luck. (Asserting these
        // against the exit-3 child of part 2 is what made this vector fail on
        // real HotSpot: that child is usually already gone by the time the
        // snapshot is taken.)
        // ------------------------------------------------------------------
        Process live = new ProcessBuilder(sleepLong())
                .redirectOutput(ProcessBuilder.Redirect.DISCARD)
                .redirectError(ProcessBuilder.Redirect.DISCARD)
                .start();
        ProcessHandle lh = live.toHandle();
        check(lh.pid() == live.pid(), "Process.pid must agree with its handle");
        check(lh.pid() != ProcessHandle.current().pid(), "the child is a different process");
        check(live.isAlive(), "the sleeper must be alive");
        check(lh.isAlive(), "the sleeper's handle must agree that it is alive");
        check(ProcessHandle.of(lh.pid()).isPresent(), "of(pid) must find a live child");
        check(ProcessHandle.of(lh.pid()).get().equals(lh), "of(pid) identity for a child");
        check(lh.parent().isPresent(), "a live forked child must report a parent");
        check(lh.parent().get().pid() == ProcessHandle.current().pid(),
                "the child's parent must be us");
        check(awaitInTree(lh.pid(), false), "the child must appear in our children()");
        check(awaitInTree(lh.pid(), true), "the child must appear in our descendants()");
        // `Process.descendants()` is a DIFFERENT dispatch from the
        // `ProcessHandle.descendants()` polled above, and it is the one a VM
        // can get wrong without anything noticing: the JDK's concrete
        // `Process.descendants()` body is `return toHandle().descendants();`,
        // so a native that intercepts it and answers from its own idea of the
        // receiver's pid produces an empty stream -- which is exactly what a
        // childless process returns. CratonVM did that under `--jdk-only` until
        // 2026-08-12: the native registered on `java/lang/Process` read the
        // VM's own pid slot off a real `java.lang.ProcessImpl` receiver (which
        // does not override `descendants()`, so dispatch reaches it), got no
        // pid, and enumerated the descendants of -1.
        //
        // Asserted by POLLING for a non-empty answer, not by comparing two
        // snapshots. The obvious form -- assert that `live.descendants()` and
        // `lh.descendants()` report the same pids -- is WRONG, and real
        // HotSpot 25 rejected it: they are two separate reads of the live OS
        // process table taken microseconds apart, and this vector measured them
        // disagreeing (1 vs 2) on the ORACLE, before any CratonVM arm ran. Same
        // trap and same fix as `awaitInTree` above.
        if (windows()) {
            check(awaitOwnDescendant(live),
                    "Process.descendants() must see the sleeper's own child");
        } else {
            // `/bin/sh -c "sleep 30"` normally execs rather than forking, so
            // there is no grandchild to see and a non-empty assertion would
            // fail on HotSpot too. Named rather than silently absent -- that is
            // what `skipped` is for.
            skip("Process.descendants(): the Unix sleeper execs, so it has no descendant to see");
        }

        // The whole tree section is only meaningful if the subject never left
        // the table underneath us; prove that rather than assume it.
        check(live.isAlive(), "the sleeper must still be alive after the tree checks");

        // exitValue() on a live process is an error.
        boolean threw = false;
        try {
            live.exitValue();
        } catch (IllegalThreadStateException expected) {
            threw = true;
        }
        check(threw, "exitValue() on a live process must throw IllegalThreadStateException");
        check(!live.waitFor(50, TimeUnit.MILLISECONDS), "timed waitFor must time out");

        // destroyForcibly must actually kill it.
        live.destroyForcibly();
        check(live.waitFor(T, TimeUnit.SECONDS), "destroyForcibly did not terminate the child");
        check(!live.isAlive(), "the child must be dead after destroyForcibly");
        check(!lh.isAlive(), "the handle must be dead after destroyForcibly");
        check(live.toHandle().onExit().get(T, TimeUnit.SECONDS) != null,
                "onExit must complete for a killed child");
        // Exit codes for a killed process are platform-specific, so the VALUE
        // is deliberately not asserted or printed -- only that reading it no
        // longer throws.
        live.exitValue();

        // ------------------------------------------------------------------
        // Part 2: exit-code plumbing, on a child that exits by itself with 3.
        // Nothing here queries the process tree, so the child is free to die
        // as fast as it likes.
        // ------------------------------------------------------------------
        ProcessBuilder pb = new ProcessBuilder(exitThree());
        pb.redirectOutput(ProcessBuilder.Redirect.DISCARD);
        pb.redirectError(ProcessBuilder.Redirect.DISCARD);
        pb.redirectInput(ProcessBuilder.Redirect.INHERIT);
        Process p = pb.start();

        ProcessHandle h = p.toHandle();
        check(h.pid() == p.pid(), "Process.pid must agree with its handle");
        check(h.pid() != ProcessHandle.current().pid(), "the child is a different process");
        check(p.waitFor(T, TimeUnit.SECONDS), "the child did not exit in time");
        check(p.exitValue() == 3, "child exit code: " + p.exitValue());
        check(!p.isAlive() && !h.isAlive(), "the child must be dead after waitFor");
        check(h.onExit().get(T, TimeUnit.SECONDS) != null, "onExit must complete");
        check(p.onExit().get(T, TimeUnit.SECONDS).exitValue() == 3, "Process.onExit exit code");

        // A command that does not exist must fail, not fabricate a process.
        threw = false;
        try {
            new ProcessBuilder("cratonvm-no-such-executable-20260731").start();
        } catch (java.io.IOException expected) {
            threw = true;
        }
        check(threw, "starting a missing executable must raise IOException");

        // An EMPTY program name must fail the same way, and it is a different
        // code path: the name above reaches the OS and is refused there, this
        // one does not survive the platform ProcessImpl's own command-line
        // assembly. HotSpot 25 raises `IOException: Cannot run program "":
        // CreateProcess error=87` on Windows and
        // `error=2, No such file or directory` on Unix; both are IOExceptions,
        // which is all this asserts. Added 2026-08-12 (W7-46): under
        // `--jdk-only` the real `java.lang.ProcessImpl.create` runs, and this
        // VM's native answered a handle of 0 for an empty command line -- so
        // `start()` returned a live `Process` object that named no process, with
        // `pid() == 0`, and nothing threw.
        threw = false;
        try {
            new ProcessBuilder("").start();
        } catch (java.io.IOException expected) {
            threw = true;
        }
        check(threw, "starting an empty program name must raise IOException");
        System.out.println("CK RJdkProcess child exit=" + p.exitValue()
                + " parentIsUs=true killedThenDead=" + !live.isAlive());
    }

    static void environmentAndDirectory() throws Exception {
        ProcessBuilder pb = new ProcessBuilder(exitThree());
        pb.environment().put("CRATONVM_RJDKPROCESS", "1");
        check("1".equals(pb.environment().get("CRATONVM_RJDKPROCESS")), "environment write");
        check(pb.command().size() == exitThree().size(), "command list");
        check(pb.directory() == null, "default working directory is inherited (null)");
        pb.directory(new File(System.getProperty("java.io.tmpdir")));
        check(pb.directory() != null, "explicit working directory");
        pb.redirectOutput(ProcessBuilder.Redirect.DISCARD);
        pb.redirectError(ProcessBuilder.Redirect.DISCARD);
        Process p = pb.start();
        check(p.waitFor(T, TimeUnit.SECONDS), "env/dir child did not exit");
        check(p.exitValue() == 3, "env/dir child exit code");

        List<String> covered = new ArrayList<>(Arrays.asList(
                "current", "of", "parent", "children", "descendants", "info",
                "isAlive", "onExit", "destroyForcibly", "allProcesses"));
        Collections.sort(covered);
        System.out.println("CK RJdkProcess covered=" + covered);
    }

    public static void main(String[] args) throws Exception {
        currentProcess();
        childProcess();
        environmentAndDirectory();
        // The count is an ASSERTION now, not a printed observation. A check
        // that stops running used to move this number and nothing else --
        // exit 0, `PASS`, no exception, and the drop visible only to whoever
        // diffed the transcript against HotSpot's. See `skipped`.
        if (checks != EXPECTED_CHECKS) {
            throw new AssertionError("check count moved: expected " + EXPECTED_CHECKS
                    + ", ran " + checks + "; skipped=" + skipped);
        }
        // The count MUST stand alone: `harness_check_count` parses the rest of
        // the line as part of the number, so a combined `checks=N skipped=[]`
        // returned the string "55 skipped=[]" and guard G3 silently no-opped —
        // this vector's count has never been compared across the two VMs.
        System.out.println("CK RJdkProcess skipped=" + skipped);
        System.out.println("CK RJdkProcess checks=" + checks);
        System.out.println("PASS RJdkProcess (" + checks + " checks)");
    }
}
