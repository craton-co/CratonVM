// RMhFilterArgsType — `MethodHandles.filterArguments` takes each FILTER's
// parameter type, not the target's.
//
// The adapter used to report the target's descriptor unchanged. That is
// invisible while a filter maps a type to itself, and wrong as soon as it does
// not: Netty's `PlatformDependent0.directBufferAddress` builds
// `filterArguments(MemorySegment::address, 0, MemorySegment::ofBuffer)`, whose
// type is `(Buffer)long`, and its `invokeExact((Buffer) buf)` call site was
// refused with `expected (MemorySegment)long but found (Buffer)long`.
//
//   docs/internal/fixed-suite-bugs/netty/
//     memorysegment-ofbuffer-directbufferaddress-wrongmethodtype-FIXED-20260924.md
import java.lang.foreign.MemorySegment;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.WrongMethodTypeException;
import java.nio.Buffer;
import java.nio.ByteBuffer;

public class RMhFilterArgsType {

    static int checks = 0;

    static void ck(String key, Object value) {
        checks++;
        System.out.println("CK RMhFilterArgsType " + key + "=" + value);
    }

    interface Body { Object run() throws Throwable; }

    static String outcome(Body b) {
        try {
            return String.valueOf(b.run());
        } catch (WrongMethodTypeException e) {
            return "WrongMethodTypeException";
        } catch (Throwable t) {
            return "threw " + t.getClass().getName();
        }
    }

    public static int len(String s) { return s.length(); }
    public static String fromInt(int i) { return Integer.toString(i); }
    public static String cat(String a, long b, Object c) { return a + "|" + b + "|" + c; }
    public static long twice(int i) { return 2L * i; }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle len = l.findStatic(RMhFilterArgsType.class, "len",
                MethodType.methodType(int.class, String.class));
        MethodHandle fromInt = l.findStatic(RMhFilterArgsType.class, "fromInt",
                MethodType.methodType(String.class, int.class));

        MethodHandle f = MethodHandles.filterArguments(len, 0, fromInt);
        ck("intToString.type", f.type());
        ck("intToString.exact", outcome(() -> (int) f.invokeExact(12345)));
        ck("intToString.targetTypedCallSite", outcome(() -> (int) f.invokeExact("12345")));
        ck("intToString.invoke", outcome(() -> (int) f.invoke(7)));

        // A null filter keeps its slot; `pos` offsets the replacement; a
        // primitive-changing filter changes the slot's type.
        MethodHandle cat = l.findStatic(RMhFilterArgsType.class, "cat",
                MethodType.methodType(String.class, String.class, long.class, Object.class));
        MethodHandle twice = l.findStatic(RMhFilterArgsType.class, "twice",
                MethodType.methodType(long.class, int.class));
        MethodHandle g = MethodHandles.filterArguments(cat, 1, twice, null);
        ck("posAndNull.type", g.type());
        ck("posAndNull.exact", outcome(() -> (String) g.invokeExact("a", 21, (Object) "c")));

        // Netty's shape, on a heap buffer so it is address-free: ofBuffer then
        // address. A heap segment's address() is its offset, 0 here.
        MethodHandle ofBuffer = MethodHandles.publicLookup().findStatic(MemorySegment.class,
                "ofBuffer", MethodType.methodType(MemorySegment.class, Buffer.class));
        MethodHandle address = MethodHandles.publicLookup().findVirtual(MemorySegment.class,
                "address", MethodType.methodType(long.class));
        MethodHandle addrOfBuffer = MethodHandles.filterArguments(address, 0, ofBuffer);
        ck("addressOfBuffer.type", addrOfBuffer.type());
        Buffer heap = ByteBuffer.allocate(16);
        ck("addressOfBuffer.heap", outcome(() -> (long) addrOfBuffer.invokeExact(heap)));
        Buffer direct = ByteBuffer.allocateDirect(16);
        ck("addressOfBuffer.directNonZero",
                outcome(() -> ((long) addrOfBuffer.invokeExact(direct)) != 0L));

        System.out.println("CK RMhFilterArgsType checks=" + checks);
        System.out.println("PASS RMhFilterArgsType (" + checks + " checks)");
    }
}
