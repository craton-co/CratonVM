// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 44, lane L4
// (`i43-L4-proposal-retire-the-copywith-workarounds`, step 2): the guard for
// retiring `native_record_support_deserialization_ctr` (the
// `MH_KIND_RECORD_DESER` handle that stands in for JDK 25
// `ObjectStreamClass$RecordSupport.deserializationCtr`). Records with every
// primitive component type, reference components, a null, a nested record,
// an array component, records inside an `Object[]`, an empty record, and a
// record with a compact canonical constructor (the stream's values go
// through it).
//
// Run: javac -d out L4W44RecordDeserialization.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W44RecordDeserialization
//
// Expected HotSpot 25 output (default and -Xint, measured locally), the same
// in --compatible:
//   prims: Prims[z=true, b=-3, c=q, s=300, i=70000, j=1099511627776, f=1.5, d=-2.25]
//   prims-equal: true
//   refs: Refs[name=n, boxed=42, any=[1, 2]]
//   refs-null: Refs[name=null, boxed=null, any=null]
//   nested: Outer[p=Prims[z=true, b=-3, c=q, s=300, i=70000, j=1099511627776, f=1.5, d=-2.25], r=Refs[name=x, boxed=1, any=y], data=[4, 5]]
//   array-of-records: [Checked[n=1], Empty[], Checked[n=2]]
//   empty: Empty[]
//   checked: Checked[n=7]
//   twice: Checked[n=8] Checked[n=9]
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;
import java.util.Arrays;

public class L4W44RecordDeserialization {
    record Prims(boolean z, byte b, char c, short s, int i, long j, float f, double d) implements Serializable {
    }

    record Refs(String name, Integer boxed, Object any) implements Serializable {
    }

    record Outer(Prims p, Refs r, int[] data) implements Serializable {
        @Override
        public String toString() {
            return "Outer[p=" + p + ", r=" + r + ", data=" + Arrays.toString(data) + "]";
        }
    }

    record Checked(int n) implements Serializable {
        Checked {
            if (n < 0) {
                throw new IllegalArgumentException("negative " + n);
            }
        }
    }

    record Empty() implements Serializable {
    }

    static Object roundTrip(Object o) throws Exception {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bos)) {
            out.writeObject(o);
        }
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bos.toByteArray()))) {
            return in.readObject();
        }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            Object v = r.run();
            out = v instanceof Object[] a ? Arrays.deepToString(a) : String.valueOf(v);
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
            // Wave 45: a failing row also names where it failed (the
            // innermost cause's top frames); HotSpot fails no row, so its
            // output is unchanged.
            Throwable c = t;
            while (c.getCause() != null && c.getCause() != c) {
                c = c.getCause();
            }
            StackTraceElement[] st = c.getStackTrace();
            StringBuilder sb = new StringBuilder(out);
            if (c != t) {
                sb.append(" | cause ").append(c);
            }
            for (int i = 0; i < Math.min(8, st.length); i++) {
                sb.append("\n    at ").append(st[i]);
            }
            out = sb.toString();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) {
        Prims p = new Prims(true, (byte) -3, 'q', (short) 300, 70000, 1L << 40, 1.5f, -2.25);
        row("prims", () -> roundTrip(p));
        row("prims-equal", () -> roundTrip(p).equals(p));
        row("refs", () -> roundTrip(new Refs("n", 42, java.util.List.of(1, 2))));
        row("refs-null", () -> roundTrip(new Refs(null, null, null)));
        row("nested", () -> roundTrip(new Outer(p, new Refs("x", 1, "y"), new int[] {4, 5})));
        row("array-of-records", () -> roundTrip(new Object[] {new Checked(1), new Empty(), new Checked(2)}));
        row("empty", () -> roundTrip(new Empty()));
        row("checked", () -> roundTrip(new Checked(7)));
        row("twice", () -> {
            Object a = roundTrip(new Checked(8));
            Object b = roundTrip(new Checked(9));
            return a + " " + b;
        });
    }
}
