// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.math.BigDecimal;
import java.math.BigInteger;
import java.util.Base64;
import java.util.Random;
import java.util.UUID;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * SBR round 2, C2-path audit of the reviewed {@code Intrinsic} rows: one micro row per
 * intrinsic family, median ns/op over rounds. Run the SAME class under
 * {@code CRATONVM_AUDIT_YIELD_INTRINSIC=<prefix>} (JDK bytecode) and without it (intrinsic),
 * each under the default tier policy, {@code CRATONVM_C2_ACCEPT=always} and {@code =never}.
 *
 * <pre>cratonvm --jdk-only -cp . Sbr2AuditMicro [rounds] [rows,comma]</pre>
 */
public class Sbr2AuditMicro {
    static long sink;

    interface Row { long run(int n); }

    static final byte[] B8 = new byte[4096];
    static final Pattern PAT = Pattern.compile("a(b+)c");
    static final BigInteger BA = new BigInteger(1024, new Random(1)), BB = new BigInteger(1024, new Random(2));
    static final BigInteger MOD = new BigInteger(1024, new Random(3)).setBit(0).setBit(1023);
    static final BigDecimal DA = new BigDecimal("12345.6789"), DB = new BigDecimal("0.000123");

    public static void main(String[] a) {
        int rounds = a.length > 0 ? Integer.parseInt(a[0]) : 5;
        String want = a.length > 1 ? "," + a[1] + "," : null;
        java.util.LinkedHashMap<String, Row> rows = new java.util.LinkedHashMap<>();
        rows.put("math.sin", n -> { double s = 0; for (int i = 0; i < n; i++) s += Math.sin(i * 0.001); return (long) s; });
        rows.put("math.pow", n -> { double s = 0; for (int i = 0; i < n; i++) s += Math.pow(1.0001, i & 1023); return (long) s; });
        rows.put("math.sqrt", n -> { double s = 0; for (int i = 0; i < n; i++) s += Math.sqrt(i); return (long) s; });
        rows.put("math.floorMod", n -> { long s = 0; for (int i = 0; i < n; i++) s += Math.floorMod(i, 7) + Math.abs(-i); return s; });
        rows.put("strictmath.sin", n -> { double s = 0; for (int i = 0; i < n; i++) s += StrictMath.sin(i * 0.001); return (long) s; });
        rows.put("integer.bits", n -> { long s = 0; for (int i = 0; i < n; i++) s += Integer.bitCount(i) + Long.numberOfTrailingZeros(i | 1024L) + Integer.reverseBytes(i); return s; });
        rows.put("integer.parse", n -> { long s = 0; for (int i = 0; i < n; i++) s += Integer.parseInt("12345") + Integer.toString(i).length(); return s; });
        rows.put("string.ops", n -> { long s = 0; String x = "the quick brown fox jumps over the lazy dog"; for (int i = 0; i < n; i++) s += x.indexOf("lazy") + x.hashCode() + (x.equals("a" + (i & 7)) ? 1 : 0) + x.charAt(i & 15); return s; });
        rows.put("string.concat", n -> { long s = 0; for (int i = 0; i < n; i++) s += ("k" + i + "v").length(); return s; });
        rows.put("character", n -> { long s = 0; for (int i = 0; i < n; i++) s += (Character.isLetter(i & 127) ? 1 : 0) + Character.toUpperCase(i & 127) + Character.getNumericValue(i & 127); return s; });
        rows.put("random.nextInt", n -> { Random r = new Random(5); long s = 0; for (int i = 0; i < n; i++) s += r.nextInt(1000) + (long) r.nextDouble(); return s; });
        rows.put("securerandom", n -> { java.security.SecureRandom r = new java.security.SecureRandom(); long s = 0; byte[] b = new byte[32]; for (int i = 0; i < n; i++) { r.nextBytes(b); s += b[0]; } return s; });
        rows.put("uuid", n -> { long s = 0; for (int i = 0; i < n; i++) s += UUID.randomUUID().hashCode() + UUID.fromString("123e4567-e89b-12d3-a456-426614174000").hashCode(); return s; });
        rows.put("base64", n -> { long s = 0; for (int i = 0; i < n; i++) { String e = Base64.getEncoder().encodeToString(B8); s += Base64.getDecoder().decode(e).length; } return s; });
        rows.put("regex.find", n -> { long s = 0; for (int i = 0; i < n; i++) { Matcher m = PAT.matcher("xx abbbc yy abc"); while (m.find()) s += m.end(); } return s; });
        rows.put("bigint.mul", n -> { long s = 0; for (int i = 0; i < n; i++) s += BA.multiply(BB).bitLength(); return s; });
        rows.put("bigint.modpow", n -> { long s = 0; for (int i = 0; i < n; i++) s += BA.modPow(BigInteger.valueOf(65537), MOD).bitLength(); return s; });
        rows.put("bigdec", n -> { long s = 0; for (int i = 0; i < n; i++) s += DA.add(DB).multiply(DA).scale() + DA.divide(DB, java.math.MathContext.DECIMAL64).precision(); return s; });
        rows.put("bytearray.data", n -> { long s = 0; for (int i = 0; i < n; i++) { try { ByteArrayOutputStream bo = new ByteArrayOutputStream(); DataOutputStream d = new DataOutputStream(bo); for (int k = 0; k < 16; k++) { d.writeInt(k); d.writeLong(k); d.writeShort(k); } DataInputStream di = new DataInputStream(new ByteArrayInputStream(bo.toByteArray())); for (int k = 0; k < 16; k++) s += di.readInt() + di.readLong() + di.readShort(); } catch (java.io.IOException e) { throw new RuntimeException(e); } } return s; });
        rows.put("class.flags", n -> { long s = 0; Class<?>[] c = { String.class, Runnable.class, int.class, int[].class }; for (int i = 0; i < n; i++) { Class<?> k = c[i & 3]; s += k.getModifiers() + (k.isInterface() ? 1 : 0) + (k.isPrimitive() ? 1 : 0); } return s; });
        rows.put("threadlocal", n -> { ThreadLocal<Integer> t = new ThreadLocal<>(); long s = 0; for (int i = 0; i < n; i++) { t.set(i); s += t.get(); } return s; });
        rows.put("aes.gcm", n -> {
            try {
                javax.crypto.Cipher c = javax.crypto.Cipher.getInstance("AES/GCM/NoPadding");
                javax.crypto.SecretKey k = new javax.crypto.spec.SecretKeySpec(new byte[16], "AES");
                long s = 0; byte[] iv = new byte[12];
                for (int i = 0; i < n; i++) { iv[0] = (byte) i; c.init(javax.crypto.Cipher.ENCRYPT_MODE, k, new javax.crypto.spec.GCMParameterSpec(128, iv)); s += c.doFinal(B8).length; }
                return s;
            } catch (Exception e) { throw new RuntimeException(e); }
        });
        rows.put("sha256", n -> {
            try { java.security.MessageDigest md = java.security.MessageDigest.getInstance("SHA-256"); long s = 0; for (int i = 0; i < n; i++) s += md.digest(B8)[0]; return s; }
            catch (Exception e) { throw new RuntimeException(e); }
        });
        rows.put("hmac", n -> {
            try { javax.crypto.Mac m = javax.crypto.Mac.getInstance("HmacSHA256"); m.init(new javax.crypto.spec.SecretKeySpec(new byte[32], "HmacSHA256")); long s = 0; for (int i = 0; i < n; i++) s += m.doFinal(B8)[0]; return s; }
            catch (Exception e) { throw new RuntimeException(e); }
        });
        int[] iters = new int[0];
        for (var e : rows.entrySet()) {
            String name = e.getKey();
            if (want != null && !want.contains("," + name + ",") && !want.contains("," + name.substring(0, name.indexOf('.') < 0 ? name.length() : name.indexOf('.')) + ",")) continue;
            Row r = e.getValue();
            int n = 2000;
            try {
                long t0 = System.nanoTime();
                sink += r.run(n);
                long per = Math.max(1, (System.nanoTime() - t0) / n);
                // size the loop to ~150 ms
                n = (int) Math.max(200, Math.min(2_000_000, 150_000_000L / per));
                for (int w = 0; w < 4; w++) sink += r.run(n);
                long[] t = new long[rounds];
                for (int k = 0; k < rounds; k++) { long s0 = System.nanoTime(); sink += r.run(n); t[k] = (System.nanoTime() - s0) / n; }
                java.util.Arrays.sort(t);
                System.out.println(name + " " + t[rounds / 2] + " ns/op (min " + t[0] + " max " + t[rounds - 1] + ", n=" + n + ")");
            } catch (Throwable x) {
                System.out.println(name + " ERROR " + x);
            }
        }
        if (sink == 42) System.out.println("x");
    }
}
