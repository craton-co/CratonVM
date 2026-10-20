// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;

/**
 * SBR3 lane lang: a serialization round trip in a loop. Under the pure-bytecode lever with
 * {@code java.lang.reflect.Constructor} un-exempted ({@code CRATONVM_LEVER_DROP=java/lang/reflect/Constructor}),
 * RJitMultiArrayClass s20 flips from correct to {@code ClassCastException} near iteration 77
 * (the tier-up). Prints the first iteration that differs from iteration 0 and its stack.
 */
public class Sbr3LangSerialCce {
    private static final class Holder implements Serializable {
        private static final long serialVersionUID = 1L;
        double[][] data;
        Holder(int a, int b) { this.data = new double[a][b]; }
    }

    static String round(int a, int b) throws Throwable {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (ObjectOutputStream oos = new ObjectOutputStream(bos)) {
            oos.writeObject(new Holder(a, b));
        }
        try (ObjectInputStream ois = new ObjectInputStream(new ByteArrayInputStream(bos.toByteArray()))) {
            return ((Holder) ois.readObject()).data.getClass().getName();
        }
    }

    public static void main(String[] args) throws Throwable {
        String cold = round(2, 3);
        System.out.println("CK cold=" + cold);
        for (int i = 1; i < 400; i++) {
            try {
                String r = round(2, 3);
                if (!r.equals(cold)) { System.out.println("CK DIFF i=" + i + " " + r); break; }
            } catch (Throwable t) {
                System.out.println("CK THREW i=" + i + " " + t);
                StackTraceElement[] st = t.getStackTrace();
                for (int k = 0; k < Math.min(st.length, 14); k++) System.out.println("CK   at " + st[k]);
                for (Throwable c = t.getCause(); c != null; c = c.getCause()) System.out.println("CK cause " + c);
                break;
            }
        }
        System.out.println("CK done");
    }
}
