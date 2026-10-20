// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 92, lane L3: behaviour that
// `CRATONVM_REFLECT_CTOR_MINT_LEAN` (the per-VM `Class[]` / `Constructor[]`
// component ids and the uninterned tail descriptor of a `Constructor` mint)
// and `CRATONVM_BOX_WRAPPER_CID_PER_VM` (the per-VM wrapper ids of every
// native box) must not move. Output is identical with each switch unset and
// at `=1`, in both modes.
//
//   listing   getDeclaredConstructors(): the array's component, the count,
//             and each constructor's parameter list (sorted, because the
//             listing order is not specified)
//   new       getConstructor(int, String).newInstance on a constructor whose
//             descriptor is only readable from the mint (the tail slot / side
//             row), plus the no-arg and a long/double constructor
//   params    getParameterTypes() component and getExceptionTypes()
//   box       Array.get boxes of each primitive kind: wrapper class and value
//
// Run: javac -d . L3W92LeanCtorDoors.java
//      java -cp . L3W92LeanCtorDoors                                         (HotSpot)
//      cratonvm --jdk-only -cp . L3W92LeanCtorDoors
//      CRATONVM_REFLECT_CTOR_MINT_LEAN=1 CRATONVM_BOX_WRAPPER_CID_PER_VM=1 cratonvm --jdk-only -cp . L3W92LeanCtorDoors
//      (and the same pair with --compatible)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// listing component java.lang.reflect.Constructor count 4
// listing ()
// listing (int,java.lang.String)
// listing (java.lang.String[])
// listing (long,double)
// new int,String -> 42:x
// new () -> 0:none
// new long,double -> 3:2.5
// params component java.lang.Class exceptions [java.io.IOException]
// box int java.lang.Integer 100000
// box long java.lang.Long 100000
// box double java.lang.Double 2.5
// box float java.lang.Float 1.5
// box char java.lang.Character Z
// box byte java.lang.Byte -7
// box short java.lang.Short 3000
// box boolean java.lang.Boolean true
// HOTSPOT_EXPECTED_END
import java.lang.reflect.Array;
import java.lang.reflect.Constructor;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

public class L3W92LeanCtorDoors {
    public static final class Target {
        public final String text;

        public Target() {
            text = "0:none";
        }

        public Target(int a, String b) throws java.io.IOException {
            text = a + ":" + b;
        }

        public Target(long a, double b) {
            text = a + ":" + b;
        }

        Target(String[] parts) {
            text = String.join(",", parts);
        }
    }

    static String params(Constructor<?> c) {
        StringBuilder sb = new StringBuilder("(");
        Class<?>[] ps = c.getParameterTypes();
        for (int i = 0; i < ps.length; i++) {
            if (i > 0) {
                sb.append(',');
            }
            sb.append(ps[i].getTypeName());
        }
        return sb.append(')').toString();
    }

    public static void main(String[] args) throws Throwable {
        Constructor<?>[] all = Target.class.getDeclaredConstructors();
        System.out.println("listing component " + all.getClass().getComponentType().getName()
                + " count " + all.length);
        List<String> shapes = new ArrayList<>();
        for (Constructor<?> c : all) {
            shapes.add(params(c));
        }
        Collections.sort(shapes);
        for (String s : shapes) {
            System.out.println("listing " + s);
        }

        Constructor<?> is = Target.class.getConstructor(int.class, String.class);
        System.out.println("new int,String -> " + ((Target) is.newInstance(42, "x")).text);
        System.out.println("new () -> " + ((Target) Target.class.getConstructor().newInstance()).text);
        Constructor<?> ld = Target.class.getConstructor(long.class, double.class);
        System.out.println("new long,double -> " + ((Target) ld.newInstance(3L, 2.5d)).text);

        System.out.println("params component "
                + is.getParameterTypes().getClass().getComponentType().getName()
                + " exceptions " + Arrays.toString(is.getExceptionTypes()).replace("class ", ""));

        Object[] arrays = {
            new int[] {100000}, new long[] {100000L}, new double[] {2.5}, new float[] {1.5f},
            new char[] {'Z'}, new byte[] {-7}, new short[] {3000}, new boolean[] {true},
        };
        String[] kinds = {"int", "long", "double", "float", "char", "byte", "short", "boolean"};
        for (int k = 0; k < arrays.length; k++) {
            Object box = Array.get(arrays[k], 0);
            System.out.println("box " + kinds[k] + " " + box.getClass().getName() + " " + box);
        }
    }
}
