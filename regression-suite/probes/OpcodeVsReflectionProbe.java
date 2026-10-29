import java.util.*;
public class OpcodeVsReflectionProbe {
    static void row(String tag, Object o) {
        boolean iAM = o instanceof AbstractMap,        rAM = AbstractMap.class.isInstance(o);
        boolean iAC = o instanceof AbstractCollection,  rAC = AbstractCollection.class.isInstance(o);
        boolean iRA = o instanceof RandomAccess,        rRA = RandomAccess.class.isInstance(o);
        System.out.printf("%-24s AbsMap %b/%b  AbsColl %b/%b  RandAcc %b/%b   %s%n",
            tag, iAM, rAM, iAC, rAC, iRA, rRA,
            ((iAM!=rAM)||(iAC!=rAC)||(iRA!=rRA)) ? "<-- OPCODE vs REFLECTION DIVERGE" : "");
    }
    public static void main(String[] a) {
        row("Map.of()", Map.of());
        row("Map.of(1)", Map.of("k","v"));
        row("Map.copyOf", Map.copyOf(new HashMap<>(Map.of("k","v"))));
        row("List.of()", List.of());
        row("List.of(3)", List.of(1,2,3));
        row("Set.of(1)", Set.of("x"));
        row("Collections.unmodifiableList", Collections.unmodifiableList(new ArrayList<>(List.of(1))));
    }
}
