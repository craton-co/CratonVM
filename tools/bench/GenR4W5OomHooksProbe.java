// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w5/thrash5 (2026-09-24): the {@code report_java_out_of_memory}
 * operator hooks — {@code -XX:+ExitOnOutOfMemoryError},
 * {@code -XX:+CrashOnOutOfMemoryError} and
 * {@code -XX:OnOutOfMemoryError=<cmd>} — which the launcher used to accept and
 * ignore ({@code docs/internal/gc/gengc-r4w4-oom-exit-and-command-hooks-ignored-FIXED-20260924.md}).
 * A LAUNCHER-level probe: what is checked is the process exit status and the
 * command's side effect (its output line), so each run is one command line.
 *
 * <p>The program raises two {@code OutOfMemoryError}s: a heap filled with live
 * {@code long[64]} blocks ("Java heap space"), then a VM-limit refusal
 * ("Requested array size exceeds VM limit"). HotSpot reports only the FIRST
 * error of the process, so the hooks fire once, for the heap-space error.
 * It prints its pid first so the {@code %p} expansion can be checked.
 *
 * <p>Commands and HotSpot's output ({@code -XX:+UseSerialGC -Xmx64m}; replace
 * {@code java -XX:+UseSerialGC} with
 * {@code cratonvm --java-home "$JDK" -XX:+UseGenerationalGC} for CratonVM;
 * {@code <pid>} is the number on the first line):
 *
 * <p>A. No hook (and {@code --compatible} unchanged):
 * <pre>
 *   java -XX:+UseSerialGC -Xmx64m -cp tools/bench GenR4W5OomHooksProbe; echo "rc=$?"
 *   pid=&lt;pid&gt;
 *   first: OutOfMemoryError "Java heap space"
 *   second: OutOfMemoryError "Requested array size exceeds VM limit"
 *   end
 *   rc=0
 * </pre>
 *
 * <p>B. Exit:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx64m -XX:+ExitOnOutOfMemoryError -cp tools/bench GenR4W5OomHooksProbe; echo "rc=$?"
 *   pid=&lt;pid&gt;
 *   Terminating due to java.lang.OutOfMemoryError: Java heap space
 *   rc=3
 * </pre>
 *
 * <p>C. Command, run once with the pid (Linux; other platforms print
 * {@code #   Executing "echo oom-hook <pid>"...}):
 * <pre>
 *   java -XX:+UseSerialGC -Xmx64m -XX:OnOutOfMemoryError="echo oom-hook %p" -cp tools/bench GenR4W5OomHooksProbe; echo "rc=$?"
 *   pid=&lt;pid&gt;
 *   #
 *   # java.lang.OutOfMemoryError: Java heap space
 *   # -XX:OnOutOfMemoryError="echo oom-hook %p"
 *   #   Executing /bin/sh -c "echo oom-hook &lt;pid&gt;"...
 *   oom-hook &lt;pid&gt;
 *   first: OutOfMemoryError "Java heap space"
 *   second: OutOfMemoryError "Requested array size exceeds VM limit"
 *   end
 *   rc=0
 * </pre>
 * The pass criterion is: exactly one {@code oom-hook} line, its number equal
 * to the {@code pid=} line, and the four program lines after it.
 *
 * <p>D. Command then exit, HotSpot's order (the command runs first):
 * <pre>
 *   java -XX:+UseSerialGC -Xmx64m -XX:OnOutOfMemoryError="echo oom-hook %p" -XX:+ExitOnOutOfMemoryError -cp tools/bench GenR4W5OomHooksProbe; echo "rc=$?"
 *   pid=&lt;pid&gt;
 *   # ... the four '#' lines of C ...
 *   oom-hook &lt;pid&gt;
 *   Terminating due to java.lang.OutOfMemoryError: Java heap space
 *   rc=3
 * </pre>
 *
 * <p>E. Crash: HotSpot prints {@code Aborting due to
 * java.lang.OutOfMemoryError: Java heap space}, a fatal-error banner and an
 * {@code hs_err_pid<pid>.log}, and dies of SIGABRT ({@code rc=134} on Linux).
 * CratonVM prints the same first line and aborts; the banner and log are its
 * own crash handler's, where one is installed. Check only the first line and
 * a non-zero, non-3 status.
 */
public final class GenR4W5OomHooksProbe {
    static final class Block {
        final Block next;
        final long[] data;

        Block(Block next) {
            this.next = next;
            this.data = new long[64];
        }
    }

    static Block blocks;

    public static void main(String[] args) {
        System.out.println("pid=" + ProcessHandle.current().pid());
        System.out.flush();
        try {
            while (true) {
                blocks = new Block(blocks);
            }
        } catch (OutOfMemoryError e) {
            blocks = null;
            System.out.println("first: OutOfMemoryError \"" + e.getMessage() + "\"");
        }
        try {
            final int[] huge = new int[Integer.MAX_VALUE];
            System.out.println("second: allocated " + huge.length);
        } catch (OutOfMemoryError e) {
            System.out.println("second: OutOfMemoryError \"" + e.getMessage() + "\"");
        }
        System.out.println("end");
    }
}
