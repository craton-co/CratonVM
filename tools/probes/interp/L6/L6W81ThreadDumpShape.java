// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 81, lane L6 -- probe for
// docs/internal/fixed-bugs/interpreter-i59-L1-sigquit-terminates-the-vm-FIXED-20261115.md
// ("Still open: the dump's exact shape"). The VM's thread dump (`SIGQUIT`,
// `jstack` and `jcmd Thread.print` print the same one) in HotSpot 25's shape:
// a timestamp, `Full thread dump <java.vm.name> (<java.vm.version>
// <java.vm.info>):`, the `Threads class SMR info:` block, thread lines
// `"name" #<threadId> [<os tid>] [daemon ]prio=<p> os_prio=.. cpu=..ms
// elapsed=..s tid=0x.. nid=<os tid> <state>  [0x..]`, frames with their
// module (`java.base@25.0.4/Native Method`), the lock lines, and
// `JNI global refs: N, weak refs: M`.
//
// The probe dumps itself with `<java.home>/bin/jcmd <own pid> Thread.print`
// and prints the dump's shape checks, then a MASKED form of the threads both
// VMs have as Java threads (main, Reference Handler, Finalizer,
// Common-Cleaner and its own four), in dump order: addresses (`0x?`), module
// versions (`@<v>`) and `LockSupport`'s line are masked; timestamps, tids,
// cpu and elapsed are checked, not printed. main prints its line only (it is
// reading jcmd's output). HotSpot's own threads (Signal Dispatcher, compiler
// threads, Attach Listener, VM Thread, GC threads) are not compared.
//
// Run, each mode (JAVA_HOME = the JDK 25 the VM boots, so its jcmd attaches):
//   cratonvm L6W81ThreadDumpShape
//   cratonvm --compatible L6W81ThreadDumpShape
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// layout: ok
// header names the VM by its properties: true
// smr length = java threads: true
// jni: ok
// "main" #=threadId:true [tid]=nid:true prio=5 shape:ok
// "Reference Handler" #=threadId:true [tid]=nid:true daemon prio=10 shape:ok
// state: waiting on condition | RUNNABLE
// at java.lang.ref.Reference.waitForReferencePendingList(java.base@<v>/Native Method)
// at java.lang.ref.Reference.processPendingReferences(java.base@<v>/Reference.java:246)
// at java.lang.ref.Reference$ReferenceHandler.run(java.base@<v>/Reference.java:208)
// "Finalizer" #=threadId:true [tid]=nid:true daemon prio=8 shape:ok
// state: in Object.wait() | WAITING (on object monitor)
// at java.lang.Object.wait0(java.base@<v>/Native Method)
// - waiting on <0x?> (a java.lang.ref.ReferenceQueue$Lock)
// at java.lang.Object.wait(java.base@<v>/Object.java:389)
// at java.lang.Object.wait(java.base@<v>/Object.java:351)
// at java.lang.ref.ReferenceQueue.remove0(java.base@<v>/ReferenceQueue.java:137)
// at java.lang.ref.ReferenceQueue.remove(java.base@<v>/ReferenceQueue.java:215)
// - locked <0x?> (a java.lang.ref.ReferenceQueue$Lock)
// at java.lang.ref.Finalizer$FinalizerThread.run(java.base@<v>/Finalizer.java:165)
// "Common-Cleaner" #=threadId:true [tid]=nid:true daemon prio=8 shape:ok
// state: in Object.wait() | TIMED_WAITING (on object monitor)
// at java.lang.Object.wait0(java.base@<v>/Native Method)
// - waiting on <0x?> (a java.lang.ref.ReferenceQueue$Lock)
// at java.lang.Object.wait(java.base@<v>/Object.java:389)
// at java.lang.ref.ReferenceQueue.remove0(java.base@<v>/ReferenceQueue.java:123)
// at java.lang.ref.ReferenceQueue.remove(java.base@<v>/ReferenceQueue.java:201)
// - locked <0x?> (a java.lang.ref.ReferenceQueue$Lock)
// at jdk.internal.ref.CleanerImpl.run(java.base@<v>/CleanerImpl.java:146)
// at java.lang.Thread.runWith(java.base@<v>/Thread.java:1487)
// at java.lang.Thread.run(java.base@<v>/Thread.java:1474)
// at jdk.internal.misc.InnocuousThread.run(java.base@<v>/InnocuousThread.java:148)
// "w81-waiter" #=threadId:true [tid]=nid:true daemon prio=5 shape:ok
// state: in Object.wait() | WAITING (on object monitor)
// at java.lang.Object.wait0(java.base@<v>/Native Method)
// - waiting on <0x?> (a java.lang.Object)
// at java.lang.Object.wait(java.base@<v>/Object.java:389)
// at java.lang.Object.wait(java.base@<v>/Object.java:351)
// at L6W81ThreadDumpShape.waiter(L6W81ThreadDumpShape.java:123)
// - locked <0x?> (a java.lang.Object)
// at L6W81ThreadDumpShape$$Lambda/0x?.run(Unknown Source)
// at java.lang.Thread.runWith(java.base@<v>/Thread.java:1487)
// at java.lang.Thread.run(java.base@<v>/Thread.java:1474)
// "w81-sleeper" #=threadId:true [tid]=nid:true prio=7 shape:ok
// state: waiting on condition | TIMED_WAITING (sleeping)
// at java.lang.Thread.sleepNanos0(java.base@<v>/Native Method)
// at java.lang.Thread.sleepNanos(java.base@<v>/Thread.java:509)
// at java.lang.Thread.sleep(java.base@<v>/Thread.java:540)
// at L6W81ThreadDumpShape.sleeper(L6W81ThreadDumpShape.java:132)
// at L6W81ThreadDumpShape$$Lambda/0x?.run(Unknown Source)
// at java.lang.Thread.runWith(java.base@<v>/Thread.java:1487)
// at java.lang.Thread.run(java.base@<v>/Thread.java:1474)
// "w81-parker" #=threadId:true [tid]=nid:true daemon prio=5 shape:ok
// state: waiting on condition | WAITING (parking)
// at jdk.internal.misc.Unsafe.park(java.base@<v>/Native Method)
// - parking to wait for  <0x?> (a L6W81ThreadDumpShape$Blocker)
// at java.util.concurrent.locks.LockSupport.park(java.base@<v>/LockSupport.java:?)
// at L6W81ThreadDumpShape.parker(L6W81ThreadDumpShape.java:139)
// at L6W81ThreadDumpShape$$Lambda/0x?.run(Unknown Source)
// at java.lang.Thread.runWith(java.base@<v>/Thread.java:1487)
// at java.lang.Thread.run(java.base@<v>/Thread.java:1474)
// "w81-blocked" #=threadId:true [tid]=nid:true daemon prio=5 shape:ok
// state: waiting for monitor entry | BLOCKED (on object monitor)
// at L6W81ThreadDumpShape.blocked(L6W81ThreadDumpShape.java:144)
// - waiting to lock <0x?> (a java.lang.Object)
// at L6W81ThreadDumpShape$$Lambda/0x?.run(Unknown Source)
// at java.lang.Thread.runWith(java.base@<v>/Thread.java:1487)
// at java.lang.Thread.run(java.base@<v>/Thread.java:1474)
// own thread ids ascending: true
// done
// HOTSPOT_EXPECTED_END
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

public class L6W81ThreadDumpShape {
    static final Object WAIT_LOCK = new Object();
    static final Object ENTER_LOCK = new Object();
    static final Blocker BLOCKER = new Blocker();

    static final class Blocker {
    }

    static void waiter() {
        synchronized (WAIT_LOCK) {
            try {
                WAIT_LOCK.wait();
            } catch (InterruptedException e) {
                // end of the probe
            }
        }
    }

    static void sleeper() {
        try {
            Thread.sleep(60_000);
        } catch (InterruptedException e) {
            // end of the probe
        }
    }

    static void parker() {
        LockSupport.park(BLOCKER);
    }

    static void blocked() {
        synchronized (ENTER_LOCK) {
            ENTER_LOCK.notifyAll();
        }
    }

    static final Pattern THREAD_LINE = Pattern.compile(
            "^\"(.*)\" #(\\d+) \\[(\\d+)\\] (daemon )?prio=(\\d+) os_prio=-?\\d+ cpu=\\d+\\.\\d\\dms"
                    + " elapsed=\\d+\\.\\d\\ds tid=0x[0-9a-f]{16} nid=(\\d+) (.+?)  \\[0x[0-9a-f]{16}\\]$");
    static final Pattern ANY_JAVA_THREAD = Pattern.compile("^\".*\" #\\d+ .*");
    static final Pattern SMR_HEAD = Pattern.compile(
            "^_java_thread_list=0x[0-9a-f]{16}, length=(\\d+), elements=\\{$");
    static final Pattern SMR_ELEMENTS = Pattern.compile("^(0x[0-9a-f]{16}, ){0,3}0x[0-9a-f]{16},?$");
    static final Pattern TIMESTAMP = Pattern.compile("^\\d{4}-\\d\\d-\\d\\d \\d\\d:\\d\\d:\\d\\d$");
    static final Pattern JNI = Pattern.compile("^JNI global refs: \\d+, weak refs: \\d+$");

    static Thread start(String name, Runnable r, boolean daemon, int priority) {
        Thread t = new Thread(r, name);
        t.setDaemon(daemon);
        t.setPriority(priority);
        t.start();
        return t;
    }

    static void awaitState(Thread t, Thread.State s) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (t.getState() != s && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
    }

    static String mask(String line) {
        return line.replaceAll("0x[0-9a-f]+", "0x?")
                .replaceAll("@[0-9][^/()]*/", "@<v>/")
                .replaceAll("LockSupport\\.java:\\d+", "LockSupport.java:?");
    }

    static String dump() throws Exception {
        String jcmd = System.getProperty("java.home") + "/bin/jcmd";
        Process p = new ProcessBuilder(jcmd, Long.toString(ProcessHandle.current().pid()), "Thread.print")
                .redirectErrorStream(true).start();
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        try (InputStream in = p.getInputStream()) {
            in.transferTo(out);
        }
        p.waitFor(30, TimeUnit.SECONDS);
        return out.toString(StandardCharsets.UTF_8);
    }

    /** The dump's layout up to its first thread line; the index of that line, or -1. */
    static int checkLayout(List<String> lines, int[] smrLength) {
        int i = 0;
        // jcmd's own `<pid>:` line.
        if (i < lines.size() && lines.get(i).matches("^\\d+:$")) {
            i++;
        }
        if (i >= lines.size() || !TIMESTAMP.matcher(lines.get(i)).matches()) {
            return -1;
        }
        i++;
        if (i >= lines.size() || !lines.get(i).startsWith("Full thread dump ")) {
            return -1;
        }
        i++;
        if (i >= lines.size() || !lines.get(i).isEmpty()) {
            return -1;
        }
        i++;
        if (i >= lines.size() || !lines.get(i).equals("Threads class SMR info:")) {
            return -1;
        }
        i++;
        Matcher head = i < lines.size() ? SMR_HEAD.matcher(lines.get(i)) : null;
        if (head == null || !head.matches()) {
            return -1;
        }
        smrLength[0] = Integer.parseInt(head.group(1));
        i++;
        int elements = 0;
        while (i < lines.size() && SMR_ELEMENTS.matcher(lines.get(i)).matches()) {
            elements += lines.get(i).split(", ").length;
            i++;
        }
        if (elements != smrLength[0] || i >= lines.size() || !lines.get(i).equals("}")) {
            return -1;
        }
        i++;
        if (i >= lines.size() || !lines.get(i).isEmpty()) {
            return -1;
        }
        i++;
        return i < lines.size() && lines.get(i).startsWith("\"") ? i : -1;
    }

    public static void main(String[] args) throws Exception {
        List<Thread> threads = new ArrayList<>();
        threads.add(start("w81-waiter", L6W81ThreadDumpShape::waiter, true, Thread.NORM_PRIORITY));
        threads.add(start("w81-sleeper", L6W81ThreadDumpShape::sleeper, false, 7));
        threads.add(start("w81-parker", L6W81ThreadDumpShape::parker, true, Thread.NORM_PRIORITY));
        // The common cleaner's thread starts lazily (HotSpot's boot starts it;
        // a VM whose boot does not still starts it here): a FileInputStream
        // registers its FileCleanable with it.
        java.io.File tmp = java.io.File.createTempFile("l6w81", ".tmp");
        tmp.deleteOnExit();
        new java.io.FileInputStream(tmp).close();
        String text;
        Thread blocked;
        Map<String, Long> ids = new HashMap<>();
        synchronized (ENTER_LOCK) {
            blocked = start("w81-blocked", L6W81ThreadDumpShape::blocked, true, Thread.NORM_PRIORITY);
            awaitState(threads.get(0), Thread.State.WAITING);
            awaitState(threads.get(1), Thread.State.TIMED_WAITING);
            awaitState(threads.get(2), Thread.State.WAITING);
            awaitState(blocked, Thread.State.BLOCKED);
            Thread.sleep(200);
            // Before the dump, while every thread is still alive (the blocked
            // one ends as soon as this block releases its monitor).
            for (Thread t : Thread.getAllStackTraces().keySet()) {
                ids.put(t.getName(), t.threadId());
            }
            for (Thread t : threads) {
                ids.put(t.getName(), t.threadId());
            }
            ids.put(blocked.getName(), blocked.threadId());
            ids.put("main", Thread.currentThread().threadId());
            text = dump();
        }

        List<String> lines = List.of(text.split("\n", -1));
        int[] smrLength = {-1};
        int first = checkLayout(lines, smrLength);
        System.out.println("layout: " + (first < 0 ? "BAD" : "ok"));
        String expectedHeader = "Full thread dump " + System.getProperty("java.vm.name") + " ("
                + System.getProperty("java.vm.version") + " " + System.getProperty("java.vm.info") + "):";
        System.out.println("header names the VM by its properties: " + lines.contains(expectedHeader));
        long javaThreads = lines.stream().filter(l -> ANY_JAVA_THREAD.matcher(l).matches()).count();
        System.out.println("smr length = java threads: " + (smrLength[0] == javaThreads));
        int jni = -1;
        for (int i = 0; i < lines.size(); i++) {
            if (JNI.matcher(lines.get(i)).matches()) {
                jni = i;
            }
        }
        System.out.println("jni: " + (jni > 0 && jni + 1 < lines.size() && lines.get(jni + 1).isEmpty()
                && lines.get(jni - 1).isEmpty() ? "ok" : "BAD"));

        List<String> wanted = List.of("main", "Reference Handler", "Finalizer", "Common-Cleaner",
                "w81-waiter", "w81-sleeper", "w81-parker", "w81-blocked");
        long lastId = -1;
        boolean ascending = true;
        boolean printing = false;
        for (int i = Math.max(first, 0); i < lines.size(); i++) {
            String line = lines.get(i);
            if (line.startsWith("\"")) {
                int end = line.indexOf('"', 1);
                String name = end > 0 ? line.substring(1, end) : "";
                printing = wanted.contains(name);
                if (!printing) {
                    continue;
                }
                Matcher m = THREAD_LINE.matcher(line);
                if (!m.matches()) {
                    System.out.println("\"" + name + "\" shape:BAD " + mask(line));
                    printing = !name.equals("main");
                    continue;
                }
                long id = Long.parseLong(m.group(2));
                // The dump order of the probe's own threads, which it creates
                // and starts in this order (the JDK's own threads' ids depend
                // on the boot; main's on this VM: see the page).
                if (name.startsWith("w81-")) {
                    ascending &= id > lastId;
                    lastId = id;
                }
                Long threadId = ids.get(name);
                System.out.println("\"" + name + "\" #=threadId:" + (threadId != null && threadId == id)
                        + " [tid]=nid:" + m.group(3).equals(m.group(6)) + " "
                        + (m.group(4) == null ? "" : "daemon ") + "prio=" + m.group(5) + " shape:ok");
                if (name.equals("main")) {
                    printing = false;
                    continue;
                }
                String state = i + 1 < lines.size() ? lines.get(i + 1).trim() : "";
                System.out.println("state: " + m.group(7) + " | "
                        + state.replaceFirst("^java\\.lang\\.Thread\\.State: ", ""));
                i++;
                continue;
            }
            if (!printing) {
                continue;
            }
            if (line.isEmpty()) {
                printing = false;
                continue;
            }
            System.out.println(mask(line.trim()));
        }
        System.out.println("own thread ids ascending: " + ascending);

        for (Thread t : threads) {
            t.interrupt();
            LockSupport.unpark(t);
        }
        blocked.join(2000);
        for (Thread t : threads) {
            t.join(2000);
        }
        System.out.println("done");
    }
}
