// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L7 -- the ANTLR native shadows
// (native-builtins/src/antlr_intrinsics.rs) must raise what ANTLR's own
// bytecode raises: an out-of-range ATNState.transition(i) is ArrayList.get's
// IndexOutOfBoundsException ("Index 5 out of bounds for length 0"), never an
// ArrayIndexOutOfBoundsException "Array index out of range: 5"; a null field
// the shadow cannot handle is the bytecode's JEP 358 NPE, never a shadow text
// such as "ATNState.transitions" or "SetTransition.set"; and a NotSetTransition
// with a null set and an out-of-vocabulary symbol answers false (the && never
// reaches super.matches) without recursing.
// See docs/internal/fixed-bugs/interpreter-i100-L7-native-shadow-npe-texts-FIXED-20261016.md.
//
// Needs the ANTLR 4.13.2 runtime (same jar for javac, HotSpot and CratonVM):
//   A=~/.m2/repository/org/antlr/antlr4-runtime/4.13.2/antlr4-runtime-4.13.2.jar
//   $J/javac -cp $A -d out tools/probes/interp/L7/L7W100AntlrShadowNulls.java
//   $B [--compatible] [--nojit] --java-home $J/.. -cp out:$A L7W100AntlrShadowNulls
//   $J/java -cp out:$A L7W100AntlrShadowNulls
//
// The NPE texts depend on how the ANTLR jar was compiled (local names), so the
// reference is HotSpot's run on the same jar. Rows that do not depend on it:
// HOTSPOT_EXPECTED_BEGIN (JDK 25, antlr4-runtime 4.13.2; NPE texts abbreviated)
// transition-out-of-range: java.lang.IndexOutOfBoundsException: Index 5 out of bounds for length 0
// transition-negative: java.lang.IndexOutOfBoundsException: Index -1 out of bounds for length 0
// transition-warm: [java.lang.IndexOutOfBoundsException: Index 5 out of bounds for length 0]
// notset-null-set-out-of-vocab: false
// notset-null-set-in-vocab: java.lang.NullPointerException: Cannot invoke "org.antlr.v4.runtime.misc.IntervalSet.contains(int)" because "this.set" is null
// set-null-set: java.lang.NullPointerException: Cannot invoke "org.antlr.v4.runtime.misc.IntervalSet.contains(int)" because "this.set" is null
// null-transitions-count: java.lang.NullPointerException: Cannot invoke "java.util.List.size()" because "this.transitions" is null
// null-transitions-get: java.lang.NullPointerException: Cannot invoke "java.util.List.get(int)" because "this.transitions" is null
// null-intervals-contains: java.lang.NullPointerException: (HotSpot's text for IntervalSet.contains)
// array-context-null-parents: java.lang.NullPointerException: (HotSpot's text for PredictionContext.calculateHashCode)
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Field;
import java.util.Set;
import java.util.TreeSet;
import org.antlr.v4.runtime.atn.ArrayPredictionContext;
import org.antlr.v4.runtime.atn.ATNState;
import org.antlr.v4.runtime.atn.BasicState;
import org.antlr.v4.runtime.atn.NotSetTransition;
import org.antlr.v4.runtime.atn.PredictionContext;
import org.antlr.v4.runtime.atn.SetTransition;
import org.antlr.v4.runtime.misc.IntervalSet;

public class L7W100AntlrShadowNulls {
    interface Row {
        Object run() throws Throwable;
    }

    static String answer(Row r) {
        try {
            return String.valueOf(r.run());
        } catch (Throwable t) {
            return t.toString();
        }
    }

    static void row(String name, Row r) {
        System.out.println(name + ": " + answer(r));
    }

    static void setField(Class<?> owner, Object target, String name, Object value) throws Exception {
        Field f = owner.getDeclaredField(name);
        f.setAccessible(true);
        f.set(target, value);
    }

    public static void main(String[] args) throws Exception {
        BasicState empty = new BasicState();
        row("transition-out-of-range", () -> empty.transition(5));
        row("transition-negative", () -> empty.transition(-1));
        Set<String> warm = new TreeSet<>();
        for (int i = 0; i < 20000; i++) {
            warm.add(answer(() -> empty.transition(5)));
        }
        System.out.println("transition-warm: " + warm);

        BasicState target = new BasicState();
        NotSetTransition notSet = new NotSetTransition(target, IntervalSet.of(3, 4));
        setField(SetTransition.class, notSet, "set", null);
        row("notset-null-set-out-of-vocab", () -> notSet.matches(50, 1, 10));
        row("notset-null-set-in-vocab", () -> notSet.matches(5, 1, 10));

        SetTransition set = new SetTransition(target, IntervalSet.of(3, 4));
        setField(SetTransition.class, set, "set", null);
        row("set-null-set", () -> set.matches(5, 1, 10));

        BasicState noList = new BasicState();
        setField(ATNState.class, noList, "transitions", null);
        row("null-transitions-count", () -> noList.getNumberOfTransitions());
        row("null-transitions-get", () -> noList.transition(0));

        IntervalSet intervals = IntervalSet.of(1, 2);
        setField(IntervalSet.class, intervals, "intervals", null);
        row("null-intervals-contains", () -> intervals.contains(1));

        row("array-context-null-parents",
            () -> new ArrayPredictionContext((PredictionContext[]) null, new int[] {1}));
    }
}
