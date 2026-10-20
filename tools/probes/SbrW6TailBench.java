// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Array;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Proxy;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import javax.crypto.KeyAgreement;
import javax.crypto.spec.IvParameterSpec;

/**
 * SBR wave 6, lane tail, timing: per-op cost of the retired surface, best of
 * {@code rounds}, ns/op (KeyAgreement rows in us/op). Prints a checksum so a
 * wrong answer cannot pass for a fast one. One process per arm; interleave
 * arms externally (A B A B ...).
 */
public class SbrW6TailBench {
    public interface Greeter {
        String greet(String n);
    }

    static final int ROUNDS = Integer.getInteger("rounds", 5);
    long fld;
    static long sink;

    public static void main(String[] a) throws Exception {
        ClassLoader cl = Greeter.class.getClassLoader();
        Class<?>[] g = {Greeter.class};
        InvocationHandler h = (p, m, x) -> m.getName().equals("greet") ? (Object) "hi" : (Object) Integer.valueOf(1);
        Field f = SbrW6TailBench.class.getDeclaredField("fld");
        Field uf = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        uf.setAccessible(true);
        sun.misc.Unsafe u = (sun.misc.Unsafe) uf.get(null);
        Greeter pr = (Greeter) Proxy.newProxyInstance(cl, g, h);
        Class<?> pc = pr.getClass();
        byte[] iv = new byte[16];
        KeyPairGenerator kg = KeyPairGenerator.getInstance("EC");
        kg.initialize(256);
        KeyPair kp1 = kg.generateKeyPair();
        KeyPair kp2 = kg.generateKeyPair();
        String[] names = {"proxy.new", "proxy.call", "proxy.isProxyClass", "proxy.getHandler", "array.newInstance",
            "unsafe.objectFieldOffset", "unsafe.arrayIndexScale", "iv.new+getIV", "ka.getInstance(us)", "ka.ecdh(us)"};
        int[] n = {20000, 200000, 200000, 200000, 200000, 200000, 500000, 200000, 500, 100};
        double[] best = new double[names.length];
        java.util.Arrays.fill(best, 1e18);
        for (int r = 0; r < ROUNDS; r++) {
            for (int k = 0; k < names.length; k++) {
                int N = n[k];
                long s = System.nanoTime();
                for (int i = 0; i < N; i++) {
                    switch (k) {
                        case 0 -> sink += Proxy.newProxyInstance(cl, g, h).hashCode() & 1;
                        case 1 -> sink += pr.greet("x").length();
                        case 2 -> sink += Proxy.isProxyClass(pc) ? 1 : 0;
                        case 3 -> sink += Proxy.getInvocationHandler(pr) == h ? 1 : 0;
                        case 4 -> sink += Array.getLength(Array.newInstance(int.class, 16));
                        case 5 -> sink += u.objectFieldOffset(f) & 1;
                        case 6 -> sink += u.arrayIndexScale(int[].class);
                        case 7 -> sink += new IvParameterSpec(iv).getIV().length;
                        case 8 -> {
                            try {
                                sink += KeyAgreement.getInstance("ECDH").getAlgorithm().length();
                            } catch (Exception e) {
                                throw new RuntimeException(e);
                            }
                        }
                        default -> {
                            try {
                                KeyAgreement ka = KeyAgreement.getInstance("ECDH");
                                ka.init(kp1.getPrivate());
                                ka.doPhase(kp2.getPublic(), true);
                                sink += ka.generateSecret().length;
                            } catch (Exception e) {
                                throw new RuntimeException(e);
                            }
                        }
                    }
                }
                double per = (System.nanoTime() - s) / (double) N;
                if (k >= 8) per /= 1000.0;
                if (per < best[k]) best[k] = per;
            }
        }
        for (int k = 0; k < names.length; k++) System.out.printf("%-26s %12.1f%n", names[k], best[k]);
        System.out.println("checksum " + sink);
    }
}
