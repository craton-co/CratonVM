import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.util.HashMap;
import java.util.Map;

/**
 * Regression: a pointer read back out of an {@code ADDRESS} slot is the address Java stored.
 *
 * {@code slot.set(ADDRESS, 0, s)} writes the pointer C reads ({@link RStoredAddress}); reading it
 * back must answer a segment whose {@code address()} is {@code s.address()}, so {@code ==},
 * {@code equals} and map keys work across the round trip as on HotSpot. Arena blocks are named by a
 * tagged handle on this VM, and the read-back used to answer the block's real address instead.
 * Covers every store and read spelling, interior pointers, a targeted layout, every arena kind, and
 * a pointer that C itself moved ({@code strsep}, where libc has it).
 */
public class RAddressReadBack {
    static int checks;

    static void ck(String name, boolean ok) {
        checks++;
        System.out.println("CK " + name + "=" + ok);
    }

    public static void main(String[] args) throws Throwable {
        try (Arena confined = Arena.ofConfined(); Arena shared = Arena.ofShared()) {
            Arena[] arenas = {confined, shared, Arena.ofAuto(), Arena.global()};
            String[] names = {"confined", "shared", "auto", "global"};
            for (int i = 0; i < arenas.length; i++) {
                Arena a = arenas[i];
                String n = names[i];
                MemorySegment s = a.allocate(64, 8);

                MemorySegment slot = a.allocateFrom(ValueLayout.ADDRESS, s);
                ck(n + ".allocateFrom", slot.get(ValueLayout.ADDRESS, 0).address() == s.address());

                MemorySegment slot2 = a.allocate(16, 8);
                slot2.set(ValueLayout.ADDRESS, 8, s.asSlice(24));
                MemorySegment back = slot2.get(ValueLayout.ADDRESS, 8);
                ck(n + ".interior", back.address() == s.address() + 24);
                ck(n + ".equals", back.equals(MemorySegment.ofAddress(s.address() + 24)));
                ck(n + ".getAtIndex",
                        slot2.getAtIndex(ValueLayout.ADDRESS, 1).address() == s.address() + 24);
                ck(n + ".varHandle",
                        ((MemorySegment) ValueLayout.ADDRESS.varHandle().get(slot2, 8L)).address()
                                == s.address() + 24);

                // One past the end still names the block.
                slot2.set(ValueLayout.ADDRESS, 0, s.asSlice(64));
                ck(n + ".end", slot2.get(ValueLayout.ADDRESS, 0).address() == s.address() + 64);

                // A targeted read: sized by the target, aligned against Java's address.
                var targeted = ValueLayout.ADDRESS.withTargetLayout(ValueLayout.JAVA_LONG);
                s.set(ValueLayout.JAVA_LONG, 8, 0x1122334455667788L);
                slot2.set(targeted, 0, s.asSlice(8));
                MemorySegment t = slot2.get(targeted, 0);
                ck(n + ".targeted", t.address() == s.address() + 8 && t.byteSize() == 8
                        && t.get(ValueLayout.JAVA_LONG, 0) == 0x1122334455667788L);

                // MemorySegment.ofAddress of a Java address round-trips too.
                slot2.set(ValueLayout.ADDRESS, 0, MemorySegment.ofAddress(s.address() + 5));
                ck(n + ".ofAddress", slot2.get(ValueLayout.ADDRESS, 0).address() == s.address() + 5);

                Map<MemorySegment, String> map = new HashMap<>();
                map.put(MemorySegment.ofAddress(s.address()), "k");
                ck(n + ".mapKey", "k".equals(map.get(slot.get(ValueLayout.ADDRESS, 0))));
            }

            // Hot: the same answer once the read is compiled.
            MemorySegment hs = confined.allocate(32, 8);
            MemorySegment hslot = confined.allocate(16, 8);
            boolean hot = true;
            for (int i = 0; i < 200_000; i++) {
                hslot.set(ValueLayout.ADDRESS, 8, hs.asSlice(i & 31));
                hot &= hslot.get(ValueLayout.ADDRESS, 8).address() == hs.address() + (i & 31);
            }
            ck("hot", hot);

            // NULL and a non-arena address are unchanged.
            MemorySegment slot = confined.allocate(8, 8);
            slot.set(ValueLayout.ADDRESS, 0, MemorySegment.NULL);
            ck("null", slot.get(ValueLayout.ADDRESS, 0).equals(MemorySegment.NULL));
            slot.set(ValueLayout.ADDRESS, 0, MemorySegment.ofAddress(0x1000));
            ck("plain", slot.get(ValueLayout.ADDRESS, 0).address() == 0x1000);

            // A pointer C moved: strsep advances *stringp past the delimiter.
            Linker linker = Linker.nativeLinker();
            var sym = linker.defaultLookup().find("strsep");
            if (sym.isPresent()) {
                MethodHandle strsep = linker.downcallHandle(sym.get(),
                        FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.ADDRESS, ValueLayout.ADDRESS));
                MemorySegment text = confined.allocateFrom("ab,cd");
                MemorySegment p = confined.allocateFrom(ValueLayout.ADDRESS, text);
                MemorySegment r = (MemorySegment) strsep.invokeExact(p, confined.allocateFrom(","));
                ck("strsep.result", r.address() == text.address());
                ck("strsep.moved", p.get(ValueLayout.ADDRESS, 0).address() == text.address() + 3);
            } else {
                ck("strsep.result", true);
                ck("strsep.moved", true);
            }
        }
        System.out.println("PASS RAddressReadBack (" + checks + " checks)");
    }
}
