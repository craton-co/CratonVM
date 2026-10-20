// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L2 -- the ArrayStoreException message of
// an `aastore` refusing a HIDDEN class's instance. HotSpot's message is
// `Klass::external_name()` of the stored value's class, which keeps a hidden
// class's `/0x<hex>` tail (`convert_hidden_name_to_java`), bare and as an
// array's bottom element -- exactly `Class.getName()`. The VM's message funnel
// (`exceptions::hotspot_vm_type_error_message`) dotted every `/`, so the tail
// read `$$Lambda.0x...`.
//
//   lambda-*     a lambda (a hidden class in --jdk-only, and in HotSpot)
//   hidden-*     an instance of a Lookup.defineHiddenClass class
//   array-*      an array whose element class is hidden
//   plain        an ordinary class, unchanged
//
// The `/0x<hex>` tail is normalised to `/0xN`; `*-equals-name` compares the
// raw message with Class.getName().
//
// Run (stdout must be the block below, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W84ArrayStoreHiddenName.java
//   $B --java-home $J/.. -cp out L2W84ArrayStoreHiddenName
//   $B --compatible --java-home $J/.. -cp out L2W84ArrayStoreHiddenName
//   (each also with --nojit)
//   $J/java -cp out L2W84ArrayStoreHiddenName
//
// Derived from the JDK 25 sources named above; refresh from $J/java before
// reading a DIFF.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// lambda L2W84ArrayStoreHiddenName$$Lambda/0xN
// lambda-equals-name true
// hidden L2W84AseHid/0xN
// hidden-equals-name true
// array [LL2W84AseHid/0xN;
// array-equals-name true
// plain java.lang.Integer
// HOTSPOT_EXPECTED_END
import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Array;

public class L2W84ArrayStoreHiddenName {
    static String norm(String s) {
        return s == null ? null : s.replaceAll("/0x[0-9a-fA-F]+", "/0xN");
    }

    static void store(String label, Object value) {
        Object[] strings = new String[1];
        try {
            strings[0] = value;
            System.out.println(label + " no-throw");
        } catch (ArrayStoreException e) {
            System.out.println(label + " " + norm(e.getMessage()));
            if (!label.equals("plain")) {
                System.out.println(label + "-equals-name " + value.getClass().getName().equals(e.getMessage()));
            }
        }
    }

    public static void main(String[] args) throws Throwable {
        Runnable lambda = () -> { };
        store("lambda", lambda);

        byte[] bytes;
        try (InputStream in = L2W84ArrayStoreHiddenName.class.getResourceAsStream("/L2W84AseHid.class")) {
            bytes = in.readAllBytes();
        }
        Class<?> hidden = MethodHandles.lookup().defineHiddenClass(bytes, true).lookupClass();
        Object instance = hidden.getDeclaredConstructor().newInstance();
        store("hidden", instance);
        store("array", Array.newInstance(hidden, 1));
        store("plain", Integer.valueOf(7));
    }
}

// Defined again as a HIDDEN class from its own bytes (a top-level class: no
// NestHost attribute).
class L2W84AseHid {
    public L2W84AseHid() {
    }
}
