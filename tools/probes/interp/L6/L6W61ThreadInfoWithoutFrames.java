// Interpreter round i2 wave 61, lane L6: `ThreadInfo`s asked without frames
// (`getThreadInfo(id)`, `getThreadInfo(ids)`, `getThreadInfo(ids, false,
// false, 0)`) of running threads, answered without a stack capture
// (docs/internal/fixed-bugs/interpreter-L6-proposal-a-thread-info-without-frames-takes-no-pause-FIXED-20261025.md,
// `CRATONVM_THREAD_INFO_FRAMELESS`).
//
// Three daemon threads: `spinner` spins in Java, `deflater` runs a long
// `Deflater.deflateBytesBytes` (16 MiB at level 9 per call), `sleeper` sleeps.
// Each row prints a thread's state, JMX state, in-native flag and frame count
// as one door reports them; the `depth 1` rows are the control (one frame,
// its method name). The three doors are polled 200 times in between (the
// loop a monitoring console runs); the answers must not change.
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box; three
// runs, the same each time):
//
//   one spinner: jmx=RUNNABLE inNative=false frames=0
//   one deflater: jmx=RUNNABLE inNative=true frames=0
//   one sleeper: jmx=TIMED_WAITING inNative=false frames=0
//   ids spinner: jmx=RUNNABLE inNative=false frames=0
//   ids deflater: jmx=RUNNABLE inNative=true frames=0
//   ids sleeper: jmx=TIMED_WAITING inNative=false frames=0
//   dump spinner: jmx=RUNNABLE inNative=false frames=0
//   dump deflater: jmx=RUNNABLE inNative=true frames=0
//   dump sleeper: jmx=TIMED_WAITING inNative=false frames=0
//   depth 1 spinner: jmx=RUNNABLE inNative=false frames=1 top=spin
//   depth 1 deflater: jmx=RUNNABLE inNative=true frames=1 top=deflateBytesBytes
//   depth 1 sleeper: jmx=TIMED_WAITING inNative=false frames=1 top=sleepNanos0
//   polled 200 changed=0
//   (the twelve rows again)
//   done
//
// On CratonVM, `CRATONVM_FRAME_TRACE=1` prints a `[THREAD_INFO_FRAMELESS]`
// line for every frameless snapshot that captured no stack (the positive
// control), and `--verbose:gc` one `door=frame-trace` pause per captured
// running thread: the 200 polls take none with the switch on.
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.zip.Deflater;

public class L6W61ThreadInfoWithoutFrames {
    static volatile boolean inDeflate;
    static volatile boolean spinning;

    static void deflate() {
        byte[] in = new byte[16 << 20];
        long seed = 12345;
        for (int i = 0; i < in.length; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            in[i] = (byte) ('a' + (int) ((seed >>> 33) & 7));
        }
        byte[] out = new byte[in.length + (1 << 20)];
        Deflater d = new Deflater(Deflater.BEST_COMPRESSION);
        while (true) {
            d.reset();
            d.setInput(in);
            d.finish();
            inDeflate = true;
            d.deflate(out);
        }
    }

    static void spin() {
        long x = 0;
        while (true) {
            spinning = true;
            x++;
            if (x == Long.MIN_VALUE) {
                System.out.println(x);
            }
        }
    }

    static void sleep() {
        while (true) {
            try {
                Thread.sleep(1_000_000L);
            } catch (InterruptedException e) {
                return;
            }
        }
    }

    static String row(String door, String name, ThreadInfo ti) {
        String s = door + " " + name + ": jmx=" + ti.getThreadState() + " inNative=" + ti.isInNative()
                + " frames=" + ti.getStackTrace().length;
        if (ti.getStackTrace().length > 0) {
            s += " top=" + ti.getStackTrace()[0].getMethodName();
        }
        return s;
    }

    static String key(ThreadInfo ti) {
        return ti.getThreadState() + "/" + ti.isInNative() + "/" + ti.getStackTrace().length;
    }

    static void rows(ThreadMXBean mx, Thread[] ts) {
        long[] ids = new long[ts.length];
        for (int i = 0; i < ts.length; i++) {
            ids[i] = ts[i].threadId();
        }
        for (Thread t : ts) {
            System.out.println(row("one", t.getName(), mx.getThreadInfo(t.threadId())));
        }
        ThreadInfo[] byIds = mx.getThreadInfo(ids);
        for (int i = 0; i < ts.length; i++) {
            System.out.println(row("ids", ts[i].getName(), byIds[i]));
        }
        ThreadInfo[] dump = mx.getThreadInfo(ids, false, false, 0);
        for (int i = 0; i < ts.length; i++) {
            System.out.println(row("dump", ts[i].getName(), dump[i]));
        }
        ThreadInfo[] one = mx.getThreadInfo(ids, 1);
        for (int i = 0; i < ts.length; i++) {
            System.out.println(row("depth 1", ts[i].getName(), one[i]));
        }
    }

    static void poll(ThreadMXBean mx, Thread[] ts) {
        long[] ids = new long[ts.length];
        for (int i = 0; i < ts.length; i++) {
            ids[i] = ts[i].threadId();
        }
        String[] first = new String[ts.length];
        long changed = 0;
        for (long n = 0; n < 200; n++) {
            ThreadInfo[] a = mx.getThreadInfo(ids);
            ThreadInfo[] b = mx.getThreadInfo(ids, false, false, 0);
            for (int i = 0; i < ts.length; i++) {
                String k = key(a[i]);
                if (first[i] == null) {
                    first[i] = k;
                }
                if (!k.equals(first[i]) || !key(b[i]).equals(first[i])
                        || !key(mx.getThreadInfo(ids[i])).equals(first[i])) {
                    changed++;
                }
            }
        }
        System.out.println("polled 200 changed=" + changed);
    }

    static Thread start(Runnable r, String name) {
        Thread t = new Thread(r, name);
        t.setDaemon(true);
        t.start();
        return t;
    }

    public static void main(String[] args) throws Exception {
        Thread spinner = start(L6W61ThreadInfoWithoutFrames::spin, "spinner");
        Thread deflater = start(L6W61ThreadInfoWithoutFrames::deflate, "deflater");
        Thread sleeper = start(L6W61ThreadInfoWithoutFrames::sleep, "sleeper");
        while (!inDeflate || !spinning || sleeper.getState() != Thread.State.TIMED_WAITING) {
            Thread.sleep(1);
        }
        Thread.sleep(300);
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        Thread[] ts = {spinner, deflater, sleeper};
        rows(mx, ts);
        poll(mx, ts);
        rows(mx, ts);
        System.out.println("done");
    }
}
