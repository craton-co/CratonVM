// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 5 (lane FFMUP): an exception escaping an FFM up-call with an address argument ends the
// VM as on HotSpot (SharedUtils.handleUncaughtException: the trace on stderr, exit status 1); the
// batched argument mint must not change that. Compare stdout and the exit status with HotSpot 25
// (stderr's first line too: `Unrecoverable uncaught exception encountered. The VM will now exit`).
//   java --enable-native-access=ALL-UNNAMED J27FfmUpcallThrows
import java.lang.foreign.*;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import static java.lang.foreign.ValueLayout.*;

public class J27FfmUpcallThrows {
    static int thrower(MemorySegment a, MemorySegment b) {
        System.out.println("fact in-upcall " + a.get(JAVA_INT, 0) + "/" + b.byteSize());
        throw new IllegalStateException("j27 upcall throws");
    }

    public static void main(String[] args) throws Throwable {
        Linker l = Linker.nativeLinker();
        FunctionDescriptor fd = FunctionDescriptor.of(JAVA_INT, ADDRESS.withTargetLayout(JAVA_INT), ADDRESS);
        MethodHandle target = MethodHandles.lookup().findStatic(J27FfmUpcallThrows.class, "thrower",
            MethodType.methodType(int.class, MemorySegment.class, MemorySegment.class));
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment stub = l.upcallStub(target, fd, arena);
            MethodHandle call = l.downcallHandle(stub, fd);
            MemorySegment x = arena.allocateFrom(JAVA_INT, 42);
            System.out.println("fact before-call true");
            int r = (int) call.invokeExact(x, x);
            System.out.println("fact after-call-reached " + r);
        }
    }
}
