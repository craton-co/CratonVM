// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.foreign.*;
import java.lang.invoke.*;

/** The primitive-only lean path and its allocating-return refusal boundary.
 * ADDRESS results must decline lean and use the native funnel. strerror's
 * storage is owned by the C library.
 */
public class ZgcFfmLeanBoundary {
    static MethodHandle strerror;
    static MethodHandle abs;
    static long expected;
    static MemorySegment address() throws Throwable {
        return (MemorySegment) strerror.invokeExact(22);
    }
    static void check() throws Throwable {
        int answer = (int) abs.invokeExact(-22);
        if (answer != 22) throw new AssertionError("bad primitive lean result");
        if (address().address() != expected) throw new AssertionError("bad ADDRESS return");
    }
    public static void main(String[] args) throws Throwable {
        String recovered = "ffmlean: recovered";
        SymbolLookup library = SymbolLookup.libraryLookup(
            System.getProperty("os.name").startsWith("Windows") ? "msvcrt" : "libc.so.6", Arena.global());
        strerror = Linker.nativeLinker().downcallHandle(library.find("strerror").orElseThrow(),
            FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.JAVA_INT));
        abs = Linker.nativeLinker().downcallHandle(library.find("abs").orElseThrow(),
            FunctionDescriptor.of(ValueLayout.JAVA_INT, ValueLayout.JAVA_INT));
        expected = address().address();
        if (expected == 0) throw new AssertionError("null strerror address");
        for (int i = 0; i < 300000; i++) check();
        for (int r = 0; r < 3; r++) {
            ZgcInfallibleAllocation.fill();
            try { check(); }
            catch (OutOfMemoryError expected) { }
            finally { ZgcInfallibleAllocation.keep = null; }
            System.gc();
            check();
        }
        System.out.println(recovered);
    }
}
