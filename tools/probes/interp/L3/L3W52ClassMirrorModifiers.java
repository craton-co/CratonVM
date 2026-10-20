// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 52, lane L3 -- `Class.getModifiers()` and the
// mirror's `modifiers` field. JDK 25's `getModifiers()` is `return modifiers;`,
// a field HotSpot fills when it makes the mirror (`Klass::modifier_flags`:
// the InnerClasses flags of a member class, `PUBLIC|FINAL|ABSTRACT` for a
// primitive, `FINAL|ABSTRACT` plus the element's access bits for an array,
// never `ACC_SUPER`). This VM used to fill it with the class file's raw
// `access_flags` and answer `getModifiers()` from a Bridge; since wave 52 the
// mirror carries HotSpot's value and, under `CRATONVM_CLASS_MODIFIERS_JDK_BODY`,
// the Bridge is not registered over a real JDK, so the JDK's body reads it
// (docs/internal/fixed-bugs/interpreter-L3-proposal-the-class-mirror-carries-hotspots-modifiers-FIXED-20261016.md).
//
// Each row prints `getModifiers()` in hex, `Modifier.toString` of it,
// `isInterface()` (JDK 25 bytecode, which reads the field too) and
// `isSynthetic()`. With the switch on, the rows are the positive control of
// the field write: the JDK body answers from the field alone. (HotSpot's
// reflection filters `Class.modifiers`, so no row reads the field directly.)
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   object, runnable, priv-nested, prot-nested-iface, pkg-nested-abstract,
//   int, void, int[][], priv-nested[], string[], enum, nested-enum, record,
//   anonymous, local, lambda, hidden, object-again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// object: 0x1 public iface=false synth=false
// runnable: 0x601 public abstract interface iface=true synth=false
// priv-nested: 0xa private static iface=false synth=false
// prot-nested-iface: 0x60c protected abstract static interface iface=true synth=false
// pkg-nested-abstract: 0x408 abstract static iface=false synth=false
// int: 0x411 public abstract final iface=false synth=false
// void: 0x411 public abstract final iface=false synth=false
// int[][]: 0x411 public abstract final iface=false synth=false
// priv-nested[]: 0x412 private abstract final iface=false synth=false
// string[]: 0x411 public abstract final iface=false synth=false
// enum: 0x4011 public final iface=false synth=false
// nested-enum: 0x4019 public static final iface=false synth=false
// record: 0x18 static final iface=false synth=false
// anonymous: 0x0  iface=false synth=false
// local: 0x0  iface=false synth=false
// lambda: 0x1010 final iface=false synth=true
// hidden: 0x0  iface=false synth=false
// object-again: 0x1 public iface=false synth=false
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W52ClassMirrorModifiers`.
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Modifier;

public class L3W52ClassMirrorModifiers {
    private static class PrivNested {
    }

    protected interface ProtNestedIface {
    }

    abstract static class PkgNestedAbstract {
    }

    public enum NestedEnum {
        A
    }

    record Rec(int x) {
    }

    static void row(String name, Class<?> c) {
        int m = c.getModifiers();
        System.out.println(name + ": 0x" + Integer.toHexString(m) + " " + Modifier.toString(m)
                + " iface=" + c.isInterface() + " synth=" + c.isSynthetic());
    }

    public static void main(String[] args) throws Throwable {
        Runnable lambda = () -> { };
        Object anonymous = new Object() { };
        class Local {
        }
        byte[] hiddenBytes;
        try (var in = L3W52ClassMirrorModifiers.class.getResourceAsStream("L3W52HiddenSource.class")) {
            hiddenBytes = in.readAllBytes();
        }
        Class<?> hidden = MethodHandles.lookup().defineHiddenClass(hiddenBytes, false).lookupClass();

        row("object", Object.class);
        row("runnable", Runnable.class);
        row("priv-nested", PrivNested.class);
        row("prot-nested-iface", ProtNestedIface.class);
        row("pkg-nested-abstract", PkgNestedAbstract.class);
        row("int", int.class);
        row("void", void.class);
        row("int[][]", int[][].class);
        row("priv-nested[]", PrivNested[].class);
        row("string[]", String[].class);
        row("enum", java.time.DayOfWeek.class);
        row("nested-enum", NestedEnum.class);
        row("record", Rec.class);
        row("anonymous", anonymous.getClass());
        row("local", Local.class);
        row("lambda", lambda.getClass());
        row("hidden", hidden);
        row("object-again", Object.class);
    }
}

// The bytes of a hidden class (`defineHiddenClass`): a top-level class, so
// its class file has no InnerClasses entry naming itself.
class L3W52HiddenSource {
}
