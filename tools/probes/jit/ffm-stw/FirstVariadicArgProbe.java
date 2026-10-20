// SPDX-License-Identifier: Apache-2.0
// Verify the shipping downcallHandle row honors a real JDK variadic option.
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodType;
import java.util.List;
import java.util.Optional;

public final class FirstVariadicArgProbe {
    public static void main(String[] args) throws Throwable {
        var intView = ValueLayout.JAVA_INT.varHandle();
        if (intView.varType() != int.class
                || !intView.coordinateTypes().equals(List.of(MemorySegment.class, long.class))) {
            throw new AssertionError("unexpected int layout VarHandle metadata: "
                    + intView.varType() + " " + intView.coordinateTypes());
        }
        var addressView = ValueLayout.ADDRESS.varHandle();
        if (addressView.varType() != MemorySegment.class
                || !addressView.coordinateTypes().equals(List.of(MemorySegment.class, long.class))) {
            throw new AssertionError("unexpected address layout VarHandle metadata: "
                    + addressView.varType() + " " + addressView.coordinateTypes());
        }

        Linker linker = Linker.nativeLinker();
        Optional<MemorySegment> symbol = linker.defaultLookup().find("snprintf");
        if (symbol.isEmpty()) {
            throw new AssertionError("defaultLookup did not find snprintf");
        }
        FunctionDescriptor descriptor = FunctionDescriptor.of(
                ValueLayout.JAVA_INT,
                ValueLayout.ADDRESS,
                ValueLayout.JAVA_LONG,
                ValueLayout.ADDRESS,
                ValueLayout.JAVA_DOUBLE);
        MethodHandle handle = linker.downcallHandle(
                symbol.get(), descriptor, Linker.Option.firstVariadicArg(3));
        if (!handle.type().equals(MethodType.methodType(
                int.class,
                MemorySegment.class,
                long.class,
                MemorySegment.class,
                double.class))) {
            throw new AssertionError("unexpected downcall type: " + handle.type());
        }

        try (Arena arena = Arena.ofConfined()) {
            MemorySegment buffer = arena.allocate(32);
            MemorySegment format = arena.allocateFrom("%.3f");
            int written = (int) handle.invokeExact(buffer, 32L, format, 2.5d);
            String rendered = buffer.getString(0);
            if (written != 5 || !"2.500".equals(rendered)) {
                throw new AssertionError("snprintf returned " + written + " and rendered "
                        + rendered);
            }
            System.out.println("variadic result: " + rendered + " n=" + written);
            System.out.println("PASS");
        }
    }
}
