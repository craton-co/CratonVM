// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L4: `VarHandle.getAndAdd` on a `byte`,
// `short` or `char` variable wraps the stored sum to the variable's type
// (JDK 25: `Unsafe.getAndAddByte` stores `(byte) (v + delta)`, likewise
// `Short` / `Char`). CratonVM's door (`lang_invoke::varhandle_get_and_add_raw`)
// added two `int`s and stored the unnarrowed sum, so by reading a `byte` 127
// plus 1 left 128 in a static (and in a `Value`-cell instance field), and a
// `char` 65535 plus 1 left 65536. Fixed by narrowing the sum to the variable's
// descriptor (not built or run by the lane).
//
// Each row prints the returned old value, then the variable read back by
// plain bytecode and by the handle. Instance, static and array variables;
// the Acquire / Release variants too.
//
// Run: javac -d . L4W102VarHandleSubwordGetAndAdd.java
//      java -cp . L4W102VarHandleSubwordGetAndAdd                          (HotSpot)
//      cratonvm --jdk-only [--nojit] -cp . L4W102VarHandleSubwordGetAndAdd
//      cratonvm --compatible [--nojit] -cp . L4W102VarHandleSubwordGetAndAdd
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// inst-byte old 127 now -128 vh -128
// inst-short old 32767 now -32768 vh -32768
// inst-char old 65535 now 0 vh 0
// inst-byte-down old -128 now 127 vh 127
// static-byte old 127 now -128 vh -128
// static-short old -32768 now 32767 vh 32767
// static-char old 65535 now 0 vh 0
// array-byte old 127 now -128 vh -128
// array-short old 32767 now -32768 vh -32768
// array-char old 65535 now 0 vh 0
// acquire-byte old 127 now -128
// release-char old 65535 now 0
// loop-byte -128 sum 0
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L4W102VarHandleSubwordGetAndAdd {
    static class Holder {
        byte b = 127;
        short s = 32767;
        char c = 65535;
        byte d = -128;
        byte e = 127;
        char f = 65535;
        byte g = 0;
    }

    static byte SB = 127;
    static short SS = -32768;
    static char SC = 65535;

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        VarHandle hb = l.findVarHandle(Holder.class, "b", byte.class);
        VarHandle hs = l.findVarHandle(Holder.class, "s", short.class);
        VarHandle hc = l.findVarHandle(Holder.class, "c", char.class);
        VarHandle hd = l.findVarHandle(Holder.class, "d", byte.class);
        VarHandle he = l.findVarHandle(Holder.class, "e", byte.class);
        VarHandle hf = l.findVarHandle(Holder.class, "f", char.class);
        VarHandle hg = l.findVarHandle(Holder.class, "g", byte.class);
        VarHandle sb = l.findStaticVarHandle(L4W102VarHandleSubwordGetAndAdd.class, "SB", byte.class);
        VarHandle ss = l.findStaticVarHandle(L4W102VarHandleSubwordGetAndAdd.class, "SS", short.class);
        VarHandle sc = l.findStaticVarHandle(L4W102VarHandleSubwordGetAndAdd.class, "SC", char.class);
        VarHandle ab = MethodHandles.arrayElementVarHandle(byte[].class);
        VarHandle as = MethodHandles.arrayElementVarHandle(short[].class);
        VarHandle ac = MethodHandles.arrayElementVarHandle(char[].class);
        Holder h = new Holder();

        byte ob = (byte) hb.getAndAdd(h, (byte) 1);
        System.out.println("inst-byte old " + ob + " now " + h.b + " vh " + (byte) hb.get(h));
        short os = (short) hs.getAndAdd(h, (short) 1);
        System.out.println("inst-short old " + os + " now " + h.s + " vh " + (short) hs.get(h));
        char oc = (char) hc.getAndAdd(h, (char) 1);
        System.out.println("inst-char old " + (int) oc + " now " + (int) h.c + " vh " + (int) (char) hc.get(h));
        byte od = (byte) hd.getAndAdd(h, (byte) -1);
        System.out.println("inst-byte-down old " + od + " now " + h.d + " vh " + (byte) hd.get(h));

        byte osb = (byte) sb.getAndAdd((byte) 1);
        System.out.println("static-byte old " + osb + " now " + SB + " vh " + (byte) sb.get());
        short oss = (short) ss.getAndAdd((short) -1);
        System.out.println("static-short old " + oss + " now " + SS + " vh " + (short) ss.get());
        char osc = (char) sc.getAndAdd((char) 1);
        System.out.println("static-char old " + (int) osc + " now " + (int) SC + " vh " + (int) (char) sc.get());

        byte[] bs = {127};
        short[] shs = {32767};
        char[] cs = {65535};
        byte oab = (byte) ab.getAndAdd(bs, 0, (byte) 1);
        System.out.println("array-byte old " + oab + " now " + bs[0] + " vh " + (byte) ab.get(bs, 0));
        short oas = (short) as.getAndAdd(shs, 0, (short) 1);
        System.out.println("array-short old " + oas + " now " + shs[0] + " vh " + (short) as.get(shs, 0));
        char oac = (char) ac.getAndAdd(cs, 0, (char) 1);
        System.out.println("array-char old " + (int) oac + " now " + (int) cs[0] + " vh " + (int) (char) ac.get(cs, 0));

        byte oe = (byte) he.getAndAddAcquire(h, (byte) 1);
        System.out.println("acquire-byte old " + oe + " now " + h.e);
        char of = (char) hf.getAndAddRelease(h, (char) 1);
        System.out.println("release-char old " + (int) of + " now " + (int) h.f);

        // 256 increments of a byte return to where they started; the value
        // after 128 of them is -128.
        int sum = 0;
        byte at128 = 0;
        for (int i = 0; i < 256; i++) {
            if (i == 128) {
                at128 = h.g;
            }
            hg.getAndAdd(h, (byte) 1);
            sum += 1;
        }
        System.out.println("loop-byte " + at128 + " sum " + (h.g + (sum - 256)));
    }
}
