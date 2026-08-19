package org.bouncycastle.pqc.crypto.test;

import java.io.BufferedReader;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.security.SecureRandom;
import java.util.HashMap;

import org.bouncycastle.crypto.AsymmetricCipherKeyPair;
import org.bouncycastle.crypto.CipherParameters;
import org.bouncycastle.crypto.params.ParametersWithContext;
import org.bouncycastle.crypto.params.ParametersWithRandom;
import org.bouncycastle.pqc.crypto.haetae.HAETAEKeyGenerationParameters;
import org.bouncycastle.pqc.crypto.haetae.HAETAEKeyPairGenerator;
import org.bouncycastle.pqc.crypto.haetae.HAETAEParameters;
import org.bouncycastle.pqc.crypto.haetae.HAETAEPrivateKeyParameters;
import org.bouncycastle.pqc.crypto.haetae.HAETAEPublicKeyParameters;
import org.bouncycastle.pqc.crypto.haetae.HAETAESigner;
import org.bouncycastle.util.Arrays;
import org.bouncycastle.util.encoders.Hex;
import org.bouncycastle.test.TestResourceFinder;
import org.bouncycastle.util.test.FixedSecureRandom;

/**
 * HAETAETest cannot be compared across VMs as written: its TestSampler seeds a
 * java.util.Random from System.currentTimeMillis() and skips all but every ninth
 * KAT vector, so two runs execute DIFFERENT vectors. This runs the FIRST n vectors
 * of one file with no sampler, doing exactly what TestUtils.testTestVector does,
 * and reports each of the four KAT checks separately with per-phase timings — so a
 * cross-VM comparison is like-for-like and a failure names which value diverged.
 *
 * usage: HaetaeKat [vectors] [fileIndex 0|1|2]
 *
 * Build and run against the bc-java test tree (the classpath is relative, so the
 * cwd must be the bc-java checkout):
 *
 *   javac -d <out> -cp "$(cat bcjca-classpath.txt)" HaetaeKat.java
 *   cratonvm --java-home <jdk25> --Xmx 1g \n *       -Dbc.test.data.home=<bc-test-data> -c "<out>:$(cat bcjca-classpath.txt)" \n *       org.bouncycastle.pqc.crypto.test.HaetaeKat 6 0
 *
 * `HaetaeKat 6 0` is the repro for the JIT defect recorded in
 * bug-bcjava-pqc-53class-20260818.md: vector count=5 of mode2 reports sig=OK with
 * verifies=false under the JIT and verifies=true under --nojit.
 */
public class HaetaeKat
{
    private static final String[] FILES = new String[]{
        "PQCsignKAT_haetae_mode2.rsp",
        "PQCsignKAT_haetae_mode3.rsp",
        "PQCsignKAT_haetae_mode5.rsp",
    };

    private static final HAETAEParameters[] SETS = new HAETAEParameters[]{
        HAETAEParameters.haetae2,
        HAETAEParameters.haetae3,
        HAETAEParameters.haetae5,
    };

    public static void main(String[] args)
        throws Exception
    {
        int limit = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        int fileIndex = args.length > 1 ? Integer.parseInt(args[1]) : 0;
        String name = FILES[fileIndex];
        HAETAEParameters params = SETS[fileIndex];

        InputStream src = TestResourceFinder.findTestResource("pqc/crypto/haetae", name);
        BufferedReader bin = new BufferedReader(new InputStreamReader(src));

        String line;
        HashMap<String, String> buf = new HashMap<String, String>();
        int done = 0;
        long wall = System.currentTimeMillis();
        while (done < limit && (line = bin.readLine()) != null)
        {
            line = line.trim();
            if (line.startsWith("#"))
            {
                continue;
            }
            if (line.length() == 0)
            {
                if (buf.size() > 0)
                {
                    runOne(params, buf);
                    done++;
                }
                buf.clear();
                continue;
            }
            int a = line.indexOf("=");
            if (a > -1)
            {
                buf.put(line.substring(0, a).trim(), line.substring(a + 1).trim());
            }
        }
        System.out.println("TOTAL " + (System.currentTimeMillis() - wall) + "ms for "
            + done + " vector(s) of " + name);
    }

    private static void runOne(HAETAEParameters params, HashMap<String, String> buf)
        throws Exception
    {
        String count = (String)buf.get("count");
        byte[] seed = Hex.decode((String)buf.get("seed"));
        byte[] pk = Hex.decode((String)buf.get("pk"));
        byte[] sk = Hex.decode((String)buf.get("sk"));
        byte[] message = Hex.decode((String)buf.get("msg"));
        byte[] expected = Hex.decode((String)(buf.get("sm") == null ? buf.get("sig") : buf.get("sm")));

        SecureRandom random = new NISTSecureRandom(seed, null);
        HAETAEKeyPairGenerator kpGen = new HAETAEKeyPairGenerator();
        kpGen.init(new HAETAEKeyGenerationParameters(random, params));

        long t0 = System.currentTimeMillis();
        AsymmetricCipherKeyPair kp = kpGen.generateKeyPair();
        long t1 = System.currentTimeMillis();

        byte[] pkGot = ((HAETAEPublicKeyParameters)kp.getPublic()).getEncoded();
        byte[] skGot = ((HAETAEPrivateKeyParameters)kp.getPrivate()).getEncoded();

        byte[] rnd = new byte[32];
        byte[] ctx = new byte[1];
        random.nextBytes(rnd);
        random.nextBytes(ctx);
        byte[] pre = new byte[(ctx[0] & 0xff)];
        random.nextBytes(pre);
        byte[] context = Arrays.concatenate(ctx, pre);

        CipherParameters privParams = new ParametersWithContext(
            new ParametersWithRandom(kp.getPrivate(), new FixedSecureRandom(rnd)), context);

        HAETAESigner signer = new HAETAESigner();
        signer.init(true, privParams);
        long t2 = System.currentTimeMillis();
        byte[] sig = signer.generateSignature(message);
        long t3 = System.currentTimeMillis();

        HAETAESigner verifier = new HAETAESigner();
        verifier.init(false, new ParametersWithContext(kp.getPublic(), context));
        boolean verified = verifier.verifySignature(message, sig);
        long t4 = System.currentTimeMillis();

        System.out.println("count=" + count
            + " keygen=" + (t1 - t0) + "ms"
            + " sign=" + (t3 - t2) + "ms"
            + " verify=" + (t4 - t3) + "ms"
            + " | pk=" + verdict(pk, pkGot)
            + " sk=" + verdict(sk, skGot)
            + " sig=" + verdict(expected, sig)
            + " verifies=" + verified);
    }

    private static String verdict(byte[] expected, byte[] got)
    {
        if (Arrays.areEqual(expected, got))
        {
            return "OK";
        }
        int n = Math.min(expected.length, got.length);
        int at = -1;
        for (int i = 0; i != n; i++)
        {
            if (expected[i] != got[i])
            {
                at = i;
                break;
            }
        }
        if (at < 0)
        {
            return "LEN(" + expected.length + " vs " + got.length + ")";
        }
        return "DIFF@" + at + "/" + expected.length
            + "(" + Integer.toHexString(expected[at] & 0xff) + " vs "
            + Integer.toHexString(got[at] & 0xff) + ")";
    }
}
