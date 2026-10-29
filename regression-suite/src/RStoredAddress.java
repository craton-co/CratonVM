import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;

/**
 * Regression: a {@code MemorySegment} stored into an {@code ADDRESS} slot is the pointer C reads.
 *
 * {@code Arena.allocateFrom(ADDRESS, seg)} / {@code set(ADDRESS, off, seg)} build the
 * {@code const unsigned char **} that OpenSSL's {@code d2i_X509} takes (Tomcat's FFM OpenSSL
 * connector). {@code Unsafe.allocateMemory} blocks are named by a tagged arena handle here, and the
 * handle was stored raw: the callee faulted on a non-canonical address (SIGSEGV in {@code libcrypto}
 * for every Tomcat TLS class on Linux). Uses libc {@code strsep(char **, const char *)}, which reads
 * the stored pointer; skipped where libc has none.
 */
public class RStoredAddress {
    public static void main(String[] args) throws Throwable {
        Linker linker = Linker.nativeLinker();
        var sym = linker.defaultLookup().find("strsep");
        if (sym.isEmpty()) {
            System.out.println("CK strsep=absent");
            System.out.println("PASS RStoredAddress (1 checks)");
            return;
        }
        MethodHandle strsep = linker.downcallHandle(sym.get(),
                FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
        try (Arena a = Arena.ofConfined()) {
            MemorySegment text = a.allocateFrom("ab,cd");
            MemorySegment delim = a.allocateFrom(",");

            MemorySegment viaAllocate = a.allocateFrom(ValueLayout.ADDRESS, text);
            MemorySegment r = (MemorySegment) strsep.invokeExact(viaAllocate, delim);
            System.out.println("CK allocateFrom=" + r.reinterpret(16).getString(0));

            MemorySegment viaSet = a.allocate(8);
            viaSet.set(ValueLayout.ADDRESS, 0, text);
            r = (MemorySegment) strsep.invokeExact(viaSet, delim);
            System.out.println("CK set=" + r.reinterpret(16).getString(0));

            MemorySegment viaIndex = a.allocate(16);
            viaIndex.setAtIndex(ValueLayout.ADDRESS, 1, text);
            r = (MemorySegment) strsep.invokeExact(viaIndex.asSlice(8), delim);
            System.out.println("CK setAtIndex=" + r.reinterpret(16).getString(0));

            MemorySegment viaHandle = a.allocate(8);
            ValueLayout.ADDRESS.varHandle().set(viaHandle, 0L, text);
            r = (MemorySegment) strsep.invokeExact(viaHandle, delim);
            System.out.println("CK varHandle=" + r.reinterpret(16).getString(0));

            try {
                a.allocate(8).set(ValueLayout.ADDRESS, 0, MemorySegment.ofArray(new byte[4]));
                System.out.println("CK heap=none");
            } catch (IllegalArgumentException e) {
                System.out.println("CK heap=" + e.getMessage().startsWith("Heap segment not allowed"));
            }
        }
        System.out.println("PASS RStoredAddress (5 checks)");
    }
}
