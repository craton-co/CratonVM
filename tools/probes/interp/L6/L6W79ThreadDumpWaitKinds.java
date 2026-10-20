// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 79, lane L6 -- probe for
// docs/internal/fixed-bugs/interpreter-i63-L1-thread-dump-has-no-wait-kind-suffix-FIXED-20261111.md:
// a HotSpot 25 thread dump (`jcmd <pid> Thread.print`) prints a waiting
// thread's state with its kind and a header word that depends on it. The
// probe dumps itself with `$JAVA_HOME/bin/jcmd <own pid> Thread.print` and
// prints, for each of its four threads, the header's state words and the
// `java.lang.Thread.State:` line.
//
// Run, each mode (JAVA_HOME = the JDK 25 the VM boots, so its jcmd attaches):
//   cratonvm L6W79ThreadDumpWaitKinds
//   cratonvm --compatible L6W79ThreadDumpWaitKinds
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// w79-sleeper: waiting on condition | TIMED_WAITING (sleeping)
// w79-parker: waiting on condition | WAITING (parking)
// w79-waiter: in Object.wait() | WAITING (on object monitor)
// w79-blocked: waiting for monitor entry | BLOCKED (on object monitor)
// done
// HOTSPOT_EXPECTED_END
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;

public class L6W79ThreadDumpWaitKinds {
    static final Object WAIT_LOCK = new Object();
    static final Object ENTER_LOCK = new Object();

    static Thread daemon(String name, Runnable r) {
        Thread t = new Thread(r, name);
        t.setDaemon(true);
        t.start();
        return t;
    }

    static void awaitState(Thread t, Thread.State s) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (t.getState() != s && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
    }

    /** The header line's state words: after the `nid=` token, before `[0x`. */
    static String headerWords(String line) {
        int nid = line.indexOf(" nid=");
        if (nid < 0) {
            return "?";
        }
        String rest = line.substring(nid + 5);
        int sp = rest.indexOf(' ');
        if (sp < 0) {
            return "";
        }
        rest = rest.substring(sp + 1);
        int bracket = rest.indexOf("[0x");
        return (bracket < 0 ? rest : rest.substring(0, bracket)).trim();
    }

    public static void main(String[] args) throws Exception {
        List<Thread> threads = new ArrayList<>();
        threads.add(daemon("w79-sleeper", () -> {
            try {
                Thread.sleep(60_000);
            } catch (InterruptedException e) {
                // end of the probe
            }
        }));
        threads.add(daemon("w79-parker", () -> LockSupport.park()));
        threads.add(daemon("w79-waiter", () -> {
            synchronized (WAIT_LOCK) {
                try {
                    WAIT_LOCK.wait();
                } catch (InterruptedException e) {
                    // end of the probe
                }
            }
        }));
        Thread blocked;
        synchronized (ENTER_LOCK) {
            blocked = daemon("w79-blocked", () -> {
                synchronized (ENTER_LOCK) {
                    ENTER_LOCK.notifyAll();
                }
            });
            awaitState(threads.get(0), Thread.State.TIMED_WAITING);
            awaitState(threads.get(1), Thread.State.WAITING);
            awaitState(threads.get(2), Thread.State.WAITING);
            awaitState(blocked, Thread.State.BLOCKED);
            Thread.sleep(100);

            String jcmd = System.getProperty("java.home") + "/bin/jcmd";
            Process p = new ProcessBuilder(jcmd, Long.toString(ProcessHandle.current().pid()),
                    "Thread.print").redirectErrorStream(true).start();
            Map<String, String> rows = new LinkedHashMap<>();
            for (String n : new String[] {"w79-sleeper", "w79-parker", "w79-waiter", "w79-blocked"}) {
                rows.put(n, n + ": absent");
            }
            try (BufferedReader in = new BufferedReader(
                    new InputStreamReader(p.getInputStream(), StandardCharsets.UTF_8))) {
                String current = null;
                String header = null;
                String line;
                while ((line = in.readLine()) != null) {
                    if (line.startsWith("\"")) {
                        int end = line.indexOf('"', 1);
                        current = end > 0 ? line.substring(1, end) : null;
                        header = headerWords(line);
                    } else if (current != null && rows.containsKey(current)
                            && line.trim().startsWith("java.lang.Thread.State:")) {
                        String state = line.trim().substring("java.lang.Thread.State:".length()).trim();
                        rows.put(current, current + ": " + header + " | " + state);
                        current = null;
                    }
                }
            }
            p.waitFor(30, TimeUnit.SECONDS);
            for (String row : rows.values()) {
                System.out.println(row);
            }
        }
        for (Thread t : threads) {
            t.interrupt();
            LockSupport.unpark(t);
        }
        blocked.join(2000);
        System.out.println("done");
    }
}
