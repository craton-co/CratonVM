// SPDX-License-Identifier: Apache-2.0
// Only the sleeping worker's locals own this graph during allocation pressure.
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;

public class FfmHeldRootsProbe {
    static final boolean WINDOWS = System.getProperty("os.name").toLowerCase().contains("win");
    static final MethodHandle BLOCK = blockHandle();
    static MethodHandle blockHandle() {
        Linker linker = Linker.nativeLinker();
        if (WINDOWS) {
            SymbolLookup kernel32 = SymbolLookup.libraryLookup("kernel32.dll", Arena.global());
            return linker.downcallHandle(kernel32.find("Sleep").orElseThrow(),
                FunctionDescriptor.ofVoid(ValueLayout.JAVA_INT));
        }
        return linker.downcallHandle(linker.defaultLookup().find("poll").orElseThrow(),
            FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS,
                ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT));
    }
    static volatile boolean inC, returned;
    static volatile String failure;
    static volatile Object sink;
    static final class Node {
        final int value;
        final Node next;
        final byte[] payload;
        Node(int value, Node next) {
            this.value = value;
            this.next = next;
            this.payload = new byte[97];
            payload[0] = (byte)value;
        }
    }
    static void hold() {
        try {
            Node graph = null;
            for (int i = 0; i < 256; i++) graph = new Node(i, graph);
            Object monitor = new Object();
            synchronized (monitor) {
                inC = true;
                if (WINDOWS) {
                    BLOCK.invokeExact(12000);
                } else {
                    int rc = (int)BLOCK.invokeExact(MemorySegment.NULL, 0L, 12000);
                    if (rc != 0) throw new AssertionError("poll interrupted: " + rc);
                }
                returned = true;
                int count = 0;
                for (Node p = graph; p != null; p = p.next) {
                    if (p.value != 255 - count || p.payload[0] != (byte)p.value)
                        throw new AssertionError("graph changed at " + count);
                    count++;
                }
                if (count != 256) throw new AssertionError("graph size " + count);
                if (!Thread.holdsLock(monitor)) throw new AssertionError("monitor lost");
            }
        } catch (Throwable t) {
            failure = t.toString();
        } finally {
            returned = true;
        }
    }
    public static void main(String[] args) throws Exception {
        Thread worker = new Thread(FfmHeldRootsProbe::hold, "held-roots");
        worker.start();
        while (!inC && failure == null) Thread.sleep(1);
        Thread.sleep(200);
        boolean collectedInC = false;
        Object[] keep = new Object[64];
        for (int i = 0; i < 160000; i++) {
            byte[] a = new byte[1024];
            a[0] = (byte)i;
            keep[i & 63] = a;
            if (i == 80000) {
                System.gc();
                collectedInC = !returned;
            }
        }
        sink = keep;
        boolean allocatedInC = !returned;
        Thread.State state = worker.getState();
        worker.join();
        System.out.println("gc finished while the worker was in C: " + collectedInC);
        System.out.println("alloc finished while the worker was in C: " + allocatedInC);
        System.out.println("worker state in C: " + state);
        System.out.println("graph and monitor after C: " + (failure == null ? "ok" : failure));
        System.out.println(failure == null && collectedInC && allocatedInC
            && state == Thread.State.RUNNABLE ? "PASS" : "FAIL held roots");
    }
}
