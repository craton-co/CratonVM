// Interpreter round i2, found by the i53 retirement: a thread blocked in a
// file-I/O native on a FIFO holds every collection
// (docs/internal/fixed-bugs/gc-i53r-file-io-natives-blocked-on-a-fifo-hold-every-collection-FIXED-20261007.md).
//
// `reader` opens the FIFO `args[0]` (`mkfifo`; Linux) with
// `RandomAccessFile` (`raf-*`) or `FileInputStream` (`fis-*`) and reads one
// byte. In `*-read` modes `main` opens the writer end first, so the reader
// waits in `read`; in `*-open` modes it opens nothing, so the reader waits in
// `open`. Then `main` asks for a collection (`System.gc()`), names the
// reader's top frame, and a watchdog writes a byte after 5 s so a VM that
// waits for the reader still ends.
//
//   mkfifo /tmp/f1; java -cp out L6I53FifoReaderHoldsTheCollector /tmp/f1 raf-read
//
// HotSpot 25.0.4 (the Linux host, default flags), every mode:
//
//   gc prompt: true
//   reader top: read0(native)        (open0(native) in the *-open modes)
//   read 42
//   done
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.RandomAccessFile;

public class L6I53FifoReaderHoldsTheCollector {
    static volatile boolean reading;

    public static void main(String[] args) throws Exception {
        String fifo = args[0];
        String mode = args[1];
        boolean open = mode.endsWith("open");
        boolean raf = mode.startsWith("raf");
        Thread reader = new Thread(() -> {
            try {
                if (raf) {
                    reading = open;
                    try (RandomAccessFile f = new RandomAccessFile(fifo, "r")) {
                        reading = true;
                        System.out.println("read " + f.read());
                    }
                } else {
                    reading = open;
                    try (InputStream in = new FileInputStream(fifo)) {
                        reading = true;
                        System.out.println("read " + in.read());
                    }
                }
            } catch (Exception e) {
                System.out.println("reader failed: " + e);
            }
        }, "reader");
        reader.start();
        FileOutputStream[] out = new FileOutputStream[1];
        if (!open) {
            out[0] = new FileOutputStream(fifo);
        }
        while (!reading) {
            Thread.sleep(5);
        }
        Thread.sleep(300);
        Thread watchdog = new Thread(() -> {
            try {
                Thread.sleep(5000);
                if (out[0] == null) {
                    out[0] = new FileOutputStream(fifo);
                }
                out[0].write(42);
                out[0].flush();
            } catch (Exception e) {
                System.out.println("watchdog failed: " + e);
            }
        }, "watchdog");
        watchdog.setDaemon(true);
        watchdog.start();
        long start = System.nanoTime();
        System.gc();
        long ms = (System.nanoTime() - start) / 1_000_000;
        System.out.println("gc prompt: " + (ms < 2000));
        StackTraceElement[] st = reader.getStackTrace();
        System.out.println("reader top: " + (st.length == 0 ? "none" : st[0].getMethodName() + (st[0].isNativeMethod() ? "(native)" : "")));
        watchdog.join();
        reader.join();
        out[0].close();
        System.out.println("done");
    }
}
