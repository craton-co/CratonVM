// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Exercise the four JCA engines whose private slot map is indexed from a
// per-class base.
//
// `native-builtins/src/jca/` carries FOUR private copies of the base helper —
// `synthetic_base_offset` in signature.rs, key_factory.rs and kem.rs, plus
// `base_offset` in key_agreement.rs. All four ask `class_num_total_fields`
// UNCONDITIONALLY, with no fabricated-stub arm, which is the shape
// `cratonvm_native_api::appended_slots`' header says RATCHETS: in synthetic-JDK
// mode the first allocation fabricates a class declaring `base + width` fields,
// and the next call reads that number back as the NEW base. Two objects of one
// class then carry two different slot maps in one run, and every accessor after
// the first allocation indexes past the object.
//
// WHY THESE ASSERTIONS AND NOT `getAlgorithm()`. Most of the engine state is
// mirrored into process-wide side tables keyed on the receiver's identity hash,
// and the accessors consult the side table FIRST — so an algorithm name comes
// back correct even when the slot read is garbage, and a fixture built on
// `getAlgorithm()` would pass through the bug.
//
// `SIG_OFF_KEYOBJ` is the exception and is the reason this fixture signs rather
// than merely introspects: it holds the real EC key object for the SunEC ECDSA
// drive path, and it is deliberately a SLOT rather than a side-table entry
// because the GC scans synthetic object slots and so keeps the reference live
// and forwarded across init -> update -> sign. There is no fallback behind it.
// A base that moved by even one loses the key, and the signature round trip is
// what notices.
//
// Usage: java JcaSlotFamilies [rounds]
import java.math.BigInteger;
import java.security.KeyFactory;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.security.Signature;
import java.security.spec.X509EncodedKeySpec;
import java.util.ArrayList;
import java.util.List;
import javax.crypto.KeyAgreement;

public class JcaSlotFamilies {

    // Kept live so a collection has survivors to relocate while the natives
    // below are mid-flight.
    static final List<byte[]> keepalive = new ArrayList<>();

    static int failures = 0;
    static int skipped = 0;

    static void check(String what, boolean ok) {
        if (!ok) {
            failures++;
            System.out.println("FAIL " + what);
        }
    }

    static void churn(int n) {
        for (int i = 0; i < n; i++) {
            keepalive.add(new byte[512]);
            if (keepalive.size() > 400) {
                keepalive.remove(0);
            }
        }
    }

    // --- Signature: the SIG_OFF_KEYOBJ slot has no side table behind it ------
    static void signature(int round) throws Exception {
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("EC");
        check("kpg.getAlgorithm", "EC".equals(kpg.getAlgorithm()));
        kpg.initialize(256);
        churn(32);
        KeyPair kp = kpg.generateKeyPair();
        check("kpg produced a pair", kp != null && kp.getPrivate() != null
                && kp.getPublic() != null);

        byte[] msg = ("jca-slot-families-" + round).getBytes("UTF-8");

        Signature signer = Signature.getInstance("SHA256withECDSA");
        check("sig.getAlgorithm", "SHA256withECDSA".equals(signer.getAlgorithm()));
        signer.initSign(kp.getPrivate());
        churn(32);
        signer.update(msg);
        byte[] sig = signer.sign();
        check("sig produced bytes", sig != null && sig.length > 0);

        Signature verifier = Signature.getInstance("SHA256withECDSA");
        verifier.initVerify(kp.getPublic());
        churn(32);
        verifier.update(msg);
        // THE assertion. A ratcheted base loses the key object in the slot, and
        // the verify either throws or answers false.
        check("sig round trip verifies", verifier.verify(sig));

        // And a NEGATIVE control on the same pair: a tampered message must NOT
        // verify. Without it, an implementation that returns a constant `true`
        // would pass the line above.
        byte[] tampered = msg.clone();
        tampered[0] ^= 0x01;
        Signature neg = Signature.getInstance("SHA256withECDSA");
        neg.initVerify(kp.getPublic());
        neg.update(tampered);
        check("tampered message does NOT verify", !neg.verify(sig));
    }

    // --- KeyFactory: algorithm slot plus a real key round trip --------------
    static void keyFactory() throws Exception {
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("EC");
        kpg.initialize(256);
        KeyPair kp = kpg.generateKeyPair();

        KeyFactory kf = KeyFactory.getInstance("EC");
        check("kf.getAlgorithm", "EC".equals(kf.getAlgorithm()));
        churn(32);
        byte[] encoded = kp.getPublic().getEncoded();
        check("public key encodes", encoded != null && encoded.length > 0);
        // The key-spec round trip needs `java.security.spec.X509EncodedKeySpec`,
        // which synthetic-JDK mode has no stub for (NoSuchMethodError on its
        // constructor). That is a gap in that mode's class library, NOT the
        // defect under test, so it is COUNTED as a skip rather than allowed to
        // fail the run — and counted rather than swallowed, because a skip that
        // reads as a pass is how a family drops out of a suite.
        try {
            PublicKey back = kf.generatePublic(new X509EncodedKeySpec(encoded));
            check("kf round trip preserves the key",
                    back != null && java.util.Arrays.equals(back.getEncoded(), encoded));
        } catch (NoSuchMethodError | NoClassDefFoundError e) {
            skipped++;
        }
    }

    // --- KeyAgreement: base+1 is the algorithm slot ------------------------
    static void keyAgreement() throws Exception {
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("EC");
        kpg.initialize(256);
        KeyPair a = kpg.generateKeyPair();
        KeyPair b = kpg.generateKeyPair();

        KeyAgreement ka1 = KeyAgreement.getInstance("ECDH");
        check("ka.getAlgorithm", "ECDH".equals(ka1.getAlgorithm()));
        ka1.init(a.getPrivate());
        churn(32);
        ka1.doPhase(b.getPublic(), true);
        byte[] s1 = ka1.generateSecret();

        KeyAgreement ka2 = KeyAgreement.getInstance("ECDH");
        ka2.init(b.getPrivate());
        ka2.doPhase(a.getPublic(), true);
        byte[] s2 = ka2.generateSecret();

        check("ecdh secret is non-empty", s1 != null && s1.length > 0);
        // Both sides must derive the SAME secret. This is the assertion that
        // cannot be satisfied by returning a constant.
        check("ecdh secrets agree", java.util.Arrays.equals(s1, s2));
    }

    // --- KEM: present only on providers that ship one -----------------------
    static void kem() {
        try {
            Class<?> kemClass = Class.forName("javax.crypto.KEM");
            Object kem = kemClass.getMethod("getInstance", String.class)
                    .invoke(null, "DHKEM");
            check("kem instance is non-null", kem != null);
            Object algo = kemClass.getMethod("getAlgorithm").invoke(kem);
            check("kem.getAlgorithm", "DHKEM".equals(algo));
        } catch (ReflectiveOperationException | RuntimeException e) {
            // No DHKEM on this provider set. Counted, not silently ignored: a
            // skip that reads as a pass is how a family drops out of a suite.
            skipped++;
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        for (int r = 0; r < rounds; r++) {
            signature(r);
            keyFactory();
            keyAgreement();
            kem();
        }
        System.out.println("rounds=" + rounds + " failures=" + failures
                + " skipped=" + skipped);
        System.out.println("JCA_SLOT_FAMILIES_DONE");
        if (failures != 0) {
            System.exit(1);
        }
    }
}
