// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Craton Software Company
// Run with CRATONVM_ZGC_GENERATIONAL=1 to exercise compiled old-to-young
// field and array stores, including promotion and remembered-set scans.
public class ZgcRefStoreGateProbe {
    static class Payload { final int tag; Payload(int tag) { this.tag = tag; } }
    static class Holder { Payload value; }
    static Holder[] holders = new Holder[67];
    static Object[] array = new Object[67];
    static int[] expected = new int[67];
    static void put(Holder holder, int slot, Payload value) {
        holder.value = value;
        array[slot] = value;
    }
    public static void main(String[] args) {
        int rounds = args.length == 0 ? 40 : Integer.parseInt(args[0]);
        for (int i = 0; i < holders.length; i++) holders[i] = new Holder();
        for (int i = 0; i < 4; i++) System.gc();
        long checksum = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 2000; i++) {
                int slot = i % holders.length;
                int tag = r * 2000 + i;
                put(holders[slot], slot, new Payload(tag));
                expected[slot] = tag;
            }
            System.gc();
            for (int i = 0; i < holders.length; i++) {
                Payload a = holders[i].value;
                Payload b = (Payload) array[i];
                if (a == null || a != b || a.tag != expected[i])
                    throw new AssertionError("lost old-to-young store at " + r + ":" + i);
                checksum += a.tag;
            }
        }
        System.out.println("ZGC-STORE-OK checksum=" + checksum);
    }
}
