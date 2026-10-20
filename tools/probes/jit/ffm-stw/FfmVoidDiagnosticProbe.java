// SPDX-License-Identifier: Apache-2.0
import java.lang.foreign.*;
import java.lang.invoke.*;

/** Run with a native library built from ffm-void-diagnostic.c and MH diagnostics. */
public class FfmVoidDiagnosticProbe {
    static volatile byte[] pressure;
    static int collections;

    public static void collect() {
        for (int i = 0; i < 8; i++) pressure = new byte[1024 * 1024];
        System.gc();
        collections++;
    }

    public static void main(String[] args) throws Throwable {
        try (Arena arena = Arena.ofConfined()) {
            Linker linker = Linker.nativeLinker();
            MemorySegment function = SymbolLookup.libraryLookup(args[0], arena)
                .find("ffm_void_diagnostic").orElseThrow();
            MethodHandle collector = MethodHandles.lookup().findStatic(
                FfmVoidDiagnosticProbe.class, "collect", MethodType.methodType(void.class));
            MemorySegment callback = linker.upcallStub(
                collector, FunctionDescriptor.ofVoid(), arena);
            MethodHandle call = linker.downcallHandle(function, FunctionDescriptor.ofVoid(
                ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.ADDRESS,
                ValueLayout.ADDRESS, ValueLayout.ADDRESS));
            MemorySegment tiny = arena.allocate(1);
            tiny.set(ValueLayout.JAVA_BYTE, 0, (byte)42);
            call.invokeExact(callback, MemorySegment.NULL, MemorySegment.NULL,
                MemorySegment.NULL, tiny);
            if (collections != 1 || tiny.get(ValueLayout.JAVA_BYTE, 0) != 42)
                throw new AssertionError("upcall or one-byte argument was damaged");
            System.out.println("callback collected: true");
            System.out.println("tiny segment intact: true");
            System.out.println("PASS");
        }
    }
}
