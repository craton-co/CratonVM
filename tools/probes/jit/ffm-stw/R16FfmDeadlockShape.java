// SPDX-License-Identifier: Apache-2.0
// JIT round 16 wave 9 (lane ffm): the deadlock shape of
// r13w5-ffm3-a-blocked-downcall-stalls-every-stop-the-world.
//
// Thread A downcalls into a C function that blocks until thread B signals it
// (Linux: read() on a pipe; Windows: WaitForSingleObject on an event). B first
// allocates and runs System.gc(), and only then signals. HotSpot runs A's
// downcall in _thread_in_native, so B's collection does not wait for A and the
// program ends. CratonVM with the downcall counted (CRATONVM_FFM_DOWNCALL_GC_SAFE
// off) never ends: B's pause waits for A, A waits for B (rc 124 under a timeout:
// the defect, as evidence). With the flag on (either transition) it prints the
// HotSpot lines.
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;

public class R16FfmDeadlockShape {
    static volatile boolean inC;
    static volatile int aResult = -100;
    static volatile Object sink;

    public static void main(String[] args) throws Throwable {
        boolean windows = System.getProperty("os.name").toLowerCase().contains("win");
        Linker linker = Linker.nativeLinker();
        SymbolLookup std = linker.defaultLookup();
        MethodHandle block;
        MethodHandle signal;
        final MemorySegment token;
        Arena arena = Arena.ofShared();
        if (windows) {
            SymbolLookup k32 = SymbolLookup.libraryLookup("kernel32", arena);
            MethodHandle create = linker.downcallHandle(k32.find("CreateEventW").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS,
                            ValueLayout.JAVA_INT, ValueLayout.JAVA_INT, ValueLayout.ADDRESS));
            token = (MemorySegment) create.invokeExact(MemorySegment.NULL, 1, 0, MemorySegment.NULL);
            block = linker.downcallHandle(k32.find("WaitForSingleObject").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS, ValueLayout.JAVA_INT));
            signal = linker.downcallHandle(k32.find("SetEvent").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS));
        } else {
            MethodHandle pipe = linker.downcallHandle(std.find("pipe").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.ADDRESS));
            token = arena.allocate(ValueLayout.JAVA_INT, 2);
            int rc = (int) pipe.invokeExact(token);
            if (rc != 0) {
                System.out.println("pipe failed " + rc);
                return;
            }
            block = linker.downcallHandle(std.find("read").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT,
                            ValueLayout.ADDRESS, ValueLayout.JAVA_LONG));
            signal = linker.downcallHandle(std.find("write").orElseThrow(),
                    FunctionDescriptor.of(ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT,
                            ValueLayout.ADDRESS, ValueLayout.JAVA_LONG));
        }
        final MethodHandle blockCall = block;
        final MemorySegment buf = arena.allocate(8);
        Thread a = new Thread(() -> {
            try {
                inC = true;
                if (windows) {
                    aResult = (int) blockCall.invokeExact(token, -1); // INFINITE
                } else {
                    int rfd = token.getAtIndex(ValueLayout.JAVA_INT, 0);
                    aResult = (int) (long) blockCall.invokeExact(rfd, buf, 1L);
                }
            } catch (Throwable t) {
                aResult = -2;
            }
        }, "blocked-in-C");
        a.start();
        while (!inC) {
            Thread.onSpinWait();
        }
        Thread.sleep(200);
        Thread.State seen = a.getState();
        // B: collect first, then signal.
        for (int i = 0; i < 20_000; i++) {
            sink = new int[64];
        }
        System.gc();
        System.out.println("gc ran while A was blocked in C");
        System.out.println("A state while blocked in C: " + seen);
        boolean signalled;
        if (windows) {
            int ok = (int) signal.invokeExact(token);
            signalled = ok != 0;
            System.out.println("signalled " + signalled);
        } else {
            int wfd = token.getAtIndex(ValueLayout.JAVA_INT, 1);
            buf.set(ValueLayout.JAVA_BYTE, 0, (byte) 7);
            long n = (long) signal.invokeExact(wfd, buf, 1L);
            signalled = n == 1;
            System.out.println("signalled " + signalled);
        }
        a.join();
        System.out.println("A returned " + (windows ? (aResult == 0 ? "WAIT_OBJECT_0" : "" + aResult)
                : (aResult == 1 ? "1 byte" : "" + aResult)));
        System.out.println(signalled && aResult == (windows ? 0 : 1)
                && seen == Thread.State.RUNNABLE ? "PASS" : "FAIL native wait or signal");
    }
}
