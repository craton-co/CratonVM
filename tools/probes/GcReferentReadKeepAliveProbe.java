// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.ref.Reference;
import java.lang.ref.WeakReference;
import java.lang.reflect.Field;

/** Every native/handle referent-read door, with inherited receiver layouts. */
public final class GcReferentReadKeepAliveProbe {
    static final class Payload {
        final int id;
        final long check;
        final int[] data;
        Payload(int i) {
            id = i;
            check = i * 1234567L + 17;
            data = new int[] {i, i + 1, i + 2, i + 3};
        }
        boolean intact(int i) {
            return id == i && check == i * 1234567L + 17 && data != null
                    && data.length == 4 && data[0] == i && data[3] == i + 3;
        }
    }
    static final class Child extends WeakReference<Payload> {
        Object extra;
        Child(Payload p) { super(p); }
    }
    static volatile Object sink;
    static native Object readReferent(Object reference);
    static void churn(int bytes) {
        for (int i = 0; i < bytes; i += 1024) sink = new byte[1024];
    }
    public static void main(String[] args) throws Throwable {
        String door = args[0];
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        int per = args.length > 2 ? Integer.parseInt(args[2]) : 400;
        Field uf = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        uf.setAccessible(true);
        sun.misc.Unsafe u = (sun.misc.Unsafe) uf.get(null);
        Field field = Reference.class.getDeclaredField("referent");
        field.setAccessible(true);
        long offset = u.objectFieldOffset(field);
        MethodHandles.Lookup lookup = MethodHandles.privateLookupIn(Reference.class, MethodHandles.lookup());
        MethodHandle mh = lookup.unreflectGetter(field);
        VarHandle vh = lookup.findVarHandle(Reference.class, "referent", Object.class);
        Object mismatch = new Object();
        if (door.equals("jni")) System.load(args[3]);
        Object[] held = new Object[rounds * per];
        Child[] refs = new Child[held.length];
        for (int r = 0; r < rounds; r++) {
            int base = r * per;
            for (int i = 0; i < per; i++) refs[base + i] = new Child(new Payload(base + i));
            churn(2 << 20);
            for (int i = 0; i < per; i++) {
                Child ref = refs[base + i];
                Object value;
                switch (door) {
                    case "plain": value = u.getObject(ref, offset); break;
                    case "volatile": value = u.getObjectVolatile(ref, offset); break;
                    case "reflect": value = field.get(ref); break;
                    case "mh": value = (Object) mh.invokeExact((Reference) ref); break;
                    case "vh": value = vh.get(ref); break;
                    case "vhvolatile": value = vh.getVolatile(ref); break;
                    case "vhcasfail": value = vh.compareAndExchange(ref, mismatch, null); break;
                    case "jni": value = readReferent(ref); break;
                    default: throw new AssertionError(door);
                }
                if (value != null) held[base + i] = value;
            }
            churn(2 << 20);
        }
        check(door, "churn", held, refs);
        System.gc();
        check(door, "full", held, refs);
        Reference.reachabilityFence(held);
        Reference.reachabilityFence(refs);
    }
    static void check(String door, String phase, Object[] held, Child[] refs) {
        int bad = 0, cleared = 0, live = 0;
        for (int i = 0; i < held.length; i++) {
            Object value = held[i];
            if (value == null) continue;
            live++;
            if (!(value instanceof Payload) || !((Payload) value).intact(i)) bad++;
            if (refs[i].get() != value) cleared++;
        }
        System.err.println("retained=" + live);
        System.out.println(door + " " + phase + " bad=" + bad + " cleared=" + cleared);
        if (live == 0 || bad != 0 || cleared != 0) throw new AssertionError("referent lost");
    }
}
