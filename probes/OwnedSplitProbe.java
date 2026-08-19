// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// WHICH operation builds an `Owned` interpreter frame?
//
// OwnedFrameGrowthProbe showed owned frames growing at exactly one per loop
// iteration, reaching a 49.7% share -- so they are NOT the fixed startup cost
// they looked like at small scale. That kills the assumption behind boxing
// `OwnedFrameMeta` unless the driver is narrow. The loop did three things per
// iteration, so this splits them.
import java.lang.reflect.Method;
import java.util.function.IntUnaryOperator;

public final class OwnedSplitProbe {
    static int target(int x) { return x + 1; }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "lambda";
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 20_000;
        int acc = 0;
        IntUnaryOperator f = x -> x + 1;
        Method m = OwnedSplitProbe.class.getDeclaredMethod("target", int.class);

        switch (mode) {
            case "lambda":
                for (int i = 0; i < iters; i++) { acc += f.applyAsInt(i); }
                break;
            case "reflect":
                for (int i = 0; i < iters; i++) { acc += (Integer) m.invoke(null, i); }
                break;
            case "throw":
                for (int i = 0; i < iters; i++) {
                    try { throw new IllegalStateException("u"); }
                    catch (IllegalStateException e) { acc += e.getMessage().length(); }
                }
                break;
            case "plain":
                for (int i = 0; i < iters; i++) { acc += target(i); }
                break;
            default: throw new IllegalArgumentException(mode);
        }
        System.out.println(mode + " iters=" + iters + " guard=" + acc);
    }
}
