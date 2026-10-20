// SPDX-License-Identifier: Apache-2.0
import java.lang.foreign.*;
import java.util.ArrayList;
import java.util.List;

public class FfmHeapDumpProbe {
    static volatile boolean inC;
    static volatile boolean returned;
    static volatile boolean workerWasInCAtOom;
    static MethodHandleHolder holder;
    static boolean linux;
    static int nativeSleepMs;
    static final class MethodHandleHolder { java.lang.invoke.MethodHandle poll; }

    static void blockInC() throws Throwable {
        inC = true;
        if (linux) {
            int rc = (int) holder.poll.invokeExact(MemorySegment.NULL, 0L, nativeSleepMs);
            if (rc != 0) throw new AssertionError("poll=" + rc);
        } else {
            holder.poll.invokeExact(nativeSleepMs);
        }
        returned = true;
    }
    static boolean inCResult() { return workerWasInCAtOom; }
    public static void main(String[] args) throws Throwable {
        Linker linker = Linker.nativeLinker();
        holder = new MethodHandleHolder();
        linux = System.getProperty("os.name").toLowerCase().contains("linux");
        nativeSleepMs = linux ? 20000 : 25000;
        if (linux) {
            holder.poll = linker.downcallHandle(linker.defaultLookup().find("poll").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS,
                            ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT));
        } else {
            SymbolLookup kernel32 = SymbolLookup.libraryLookup("kernel32.dll", Arena.global());
            holder.poll = linker.downcallHandle(kernel32.find("Sleep").orElseThrow(),
                    FunctionDescriptor.ofVoid(ValueLayout.JAVA_INT));
        }
        Thread worker = new Thread(() -> { try { blockInC(); } catch (Throwable t) { throw new RuntimeException(t); } }, "ffm-worker");
        worker.start();
        while (!inC) Thread.sleep(1);
        // `inC` is published just before invokeExact enters the linker stub.
        // Let the downcall establish its lazy in-native state before OOM asks
        // for the no-roots heap-dump pause; otherwise the gate can race the
        // transition and test a worker that has not become claimable yet.
        Thread.sleep(200);
        List<byte[]> held = new ArrayList<>();
        try {
            while (true) held.add(new byte[1024 * 1024]);
        } catch (OutOfMemoryError expected) {
            workerWasInCAtOom = !returned;
            held.clear();
            System.gc();
            System.out.println("worker still in C at OOM: " + workerWasInCAtOom);
        }
        worker.join();
        System.out.println("worker returned: " + returned);
        System.out.println(inCResult() ? "PASS" : "FAIL waited for C");
    }
}
