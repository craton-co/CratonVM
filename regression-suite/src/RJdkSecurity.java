import java.math.BigInteger;
import java.security.KeyFactory;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.security.Provider;
import java.security.SecureRandom;
import java.security.Security;
import java.security.Signature;
import java.security.SignatureException;
import java.security.spec.PKCS8EncodedKeySpec;
import java.security.spec.X509EncodedKeySpec;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.Locale;
import javax.crypto.Cipher;
import javax.crypto.Mac;
import javax.crypto.SecretKey;
import javax.crypto.SecretKeyFactory;
import javax.crypto.spec.GCMParameterSpec;
import javax.crypto.spec.PBEKeySpec;
import javax.crypto.spec.SecretKeySpec;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLParameters;

/**
 * JDK-only corpus: security -- {@code SecureRandom}, message digests,
 * signatures, TLS where supported.
 *
 * Complements RCrypto (which pins the SHA-256/HMAC/AES-GCM/RSA KATs). This
 * vector covers PROVIDER MACHINERY: algorithm lookup, service resolution,
 * failure modes for absent algorithms, key encoding round-trips and the
 * TLS/SSLEngine surface.
 *
 * TLS: this vector goes as far as {@code SSLContext} + {@code SSLEngine}
 * construction and parameter shape, and stops there because that is its subject
 * -- NOT because a handshake is impossible here. This paragraph used to say that
 * a real loopback handshake "needs a key store, which cannot be generated
 * portably without internal APIs", and that is MEASURED FALSE (lane F36,
 * 2026-08-13): a self-signed CN=localhost certificate can be assembled as DER and
 * signed with java.security.Signature, all public API, and loaded into an
 * in-memory KeyStore that never touches the disk. RSslLiveSession does exactly
 * that and runs a loopback TLS 1.3 handshake with no resource file, no keytool
 * step and no expiry date. regression-suite/jdk-only-coverage.txt SS3 still
 * carries the old claim and is not this vector's file -- see F36-1 NOMINATION 3.
 *
 * Determinism: SecureRandom output is asserted only through invariants (length,
 * "two draws differ"), NEVER printed. Signatures over RSA use randomised PKCS#1
 * padding, so only verify() booleans are printed, never the signature bytes.
 */
public class RJdkSecurity {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** The class of the throwable {@code op} produced, or {@code "none"}. */
    static String nameOf(Throwable t) {
        return t == null ? "none" : t.getClass().getName();
    }

    /** The throwable {@code op} produced, or {@code null} — never a swallowed failure. */
    static Throwable raised(Op op) {
        try {
            op.run();
            return null;
        } catch (Throwable t) {
            return t;
        }
    }

    interface Op {
        void run() throws Exception;
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder(b.length * 2);
        for (byte x : b) {
            sb.append(Character.forDigit((x >> 4) & 0xf, 16));
            sb.append(Character.forDigit(x & 0xf, 16));
        }
        return sb.toString();
    }

    static void digests() throws Exception {
        // Known-answer tests: these are fixed by the standards, so they are safe
        // to print and are the strongest possible cross-VM check.
        MessageDigest sha256 = MessageDigest.getInstance("SHA-256");
        String empty = hex(sha256.digest(new byte[0]));
        check(empty.equals("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
                "SHA-256 of the empty input: " + empty);
        sha256.reset();
        String abc = hex(sha256.digest("abc".getBytes(java.nio.charset.StandardCharsets.UTF_8)));
        check(abc.equals("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
                "SHA-256(abc): " + abc);
        check(sha256.getDigestLength() == 32, "SHA-256 length");
        check(sha256.getAlgorithm().equals("SHA-256"), "algorithm name");

        // Incremental update must equal the one-shot digest.
        MessageDigest inc = MessageDigest.getInstance("SHA-256");
        inc.update((byte) 'a');
        inc.update("bc".getBytes(java.nio.charset.StandardCharsets.UTF_8));
        check(hex(inc.digest()).equals(abc), "incremental digest must equal one-shot");

        // clone() must fork the running state.
        MessageDigest base = MessageDigest.getInstance("SHA-256");
        base.update((byte) 'a');
        MessageDigest forked = (MessageDigest) base.clone();
        forked.update("bc".getBytes(java.nio.charset.StandardCharsets.UTF_8));
        check(hex(forked.digest()).equals(abc), "cloned digest state");

        String sha512 = hex(MessageDigest.getInstance("SHA-512").digest(new byte[0]));
        check(sha512.length() == 128, "SHA-512 hex length");
        check(sha512.startsWith("cf83e1357eefb8bd"), "SHA-512 of empty: " + sha512.substring(0, 16));

        // MessageDigest.isEqual is the constant-time comparison.
        check(MessageDigest.isEqual(new byte[] { 1, 2 }, new byte[] { 1, 2 }), "isEqual true");
        check(!MessageDigest.isEqual(new byte[] { 1, 2 }, new byte[] { 1, 3 }), "isEqual false");

        // An unknown algorithm must fail, never be fabricated.
        boolean threw = false;
        try {
            MessageDigest.getInstance("SHA-NO-SUCH-ALG");
        } catch (NoSuchAlgorithmException expected) {
            threw = true;
        }
        check(threw, "an unknown digest must raise NoSuchAlgorithmException");

        // HMAC KAT (RFC 4231 test case 2).
        Mac hmac = Mac.getInstance("HmacSHA256");
        hmac.init(new SecretKeySpec("Jefe".getBytes(java.nio.charset.StandardCharsets.UTF_8),
                "HmacSHA256"));
        String mac = hex(hmac.doFinal(
                "what do ya want for nothing?".getBytes(java.nio.charset.StandardCharsets.UTF_8)));
        check(mac.equals("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"),
                "HmacSHA256 RFC 4231 #2: " + mac);
        System.out.println("CK RJdkSecurity sha256abc=" + abc);
        System.out.println("CK RJdkSecurity hmac=" + mac);
    }

    static void secureRandoms() throws Exception {
        SecureRandom sr = new SecureRandom();
        byte[] a = new byte[32];
        byte[] b = new byte[32];
        sr.nextBytes(a);
        sr.nextBytes(b);
        check(!Arrays.equals(a, b), "two SecureRandom draws must differ");
        check(a.length == 32, "nextBytes fills the array");
        boolean allZero = true;
        for (byte x : a) {
            allZero &= x == 0;
        }
        check(!allZero, "SecureRandom must not return all zeroes");
        check(sr.getAlgorithm() != null && !sr.getAlgorithm().isEmpty(), "algorithm name");
        check(sr.getProvider() != null, "provider");
        long l1 = sr.nextLong();
        long l2 = sr.nextLong();
        check(l1 != l2, "two nextLong draws must differ");
        check(SecureRandom.getSeed(8).length == 8, "getSeed length");
        sr.setSeed(12345L);      // must be additive, never reset to a fixed stream
        byte[] c = new byte[32];
        sr.nextBytes(c);
        check(!Arrays.equals(a, c), "setSeed must not replay an earlier stream");
        check(SecureRandom.getInstanceStrong() != null, "getInstanceStrong");

        // A named PRNG algorithm, when the platform provides one.
        String named = "none";
        try {
            SecureRandom drbg = SecureRandom.getInstance("SHA1PRNG");
            byte[] d = new byte[16];
            drbg.nextBytes(d);
            named = drbg.getAlgorithm();
        } catch (NoSuchAlgorithmException e) {
            named = "absent";
        }
        check(named.equals("SHA1PRNG") || named.equals("absent"), "named PRNG: " + named);

        boolean threw = false;
        try {
            SecureRandom.getInstance("NO-SUCH-PRNG");
        } catch (NoSuchAlgorithmException expected) {
            threw = true;
        }
        check(threw, "an unknown PRNG must raise NoSuchAlgorithmException");
        System.out.println("CK RJdkSecurity prng=" + named + " distinct=true");
    }

    /**
     * {@code SecureRandom} by ARGUMENT KIND — the axis {@code secureRandoms()} above never
     * drives, because every call it makes passes a valid argument.
     *
     * <p>NOM F12-1. Lane F12 fixed seven divergences in
     * {@code native-builtins/src/securerandom.rs} and found that <b>five of the seven have
     * no regression check anywhere in the tree</b>: the {@code java.util.Random} half of
     * the identical null-argument defect had a check (RJdkIntrinsics2 {@code --only=random}
     * check 41) and was therefore caught, and the {@code SecureRandom} half had none and
     * was found only by reading the file beside it. A green {@code --only=random} is not
     * evidence for any of this — that family drives {@code java.util.Random} only.
     *
     * <p>Every expected value below was measured on Microsoft OpenJDK 25.0.3+9-LTS
     * (scratchpad/f25) and cross-checked against the JDK 25 line that produces it. The
     * arms are ordered by argument kind rather than by method, which is what turned up
     * {@link #getInstanceArgumentOrder} — a divergence that exists only when TWO arguments
     * are bad at once and that no per-method review can reach.
     *
     * <p><b>Messages are asserted only where the string is the finding.</b>
     * {@code generateSeed}'s wording was the whole of one defect (the class was already
     * right), and {@code "null algorithm name"} is the string the pre-fix
     * {@code IllegalArgumentException} <i>also</i> carried, so there the class row is the
     * discriminator and the message row pins that the correct message survived the class
     * change. Where only HotSpot's message is known and CratonVM's was never measured, the
     * class alone is asserted and the message is recorded in this lane's record instead.
     */
    static void secureRandomArgumentKinds() throws Exception {
        int mark = checks;
        SecureRandom sr = new SecureRandom();

        // (1) NULLABLE REFERENCES. F12 §3.1: five sites, and all five swallowed the null
        // and carried on. A swallowed null is worse than a loud one here — nextBytes(null)
        // returning normally tells the caller its buffer was filled.
        //
        // The message is genuinely null on all three: the JDK calls the ONE-ARGUMENT
        // Objects.requireNonNull (SecureRandom.java:774/724/266), so `message: None` is
        // the faithful answer and not a shortcut. Only java.util.Random.nextBytes(null)
        // carries text, and it carries it because HotSpot's helpful-NPE synthesises one
        // from the bytecode -- there is no requireNonNull there at all.
        Throwable t = raised(() -> sr.nextBytes(null));
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "SecureRandom.nextBytes(null) must throw NullPointerException — a body that"
                        + " matches only the Object(Some(array)) arm and falls through on"
                        + " Object(None) returns normally and the caller believes its buffer"
                        + " was filled with entropy; got " + nameOf(t));
        check(t.getMessage() == null,
                "and its message is NULL — Objects.requireNonNull(bytes) with no message"
                        + " argument (SecureRandom.java:774); got " + t.getMessage());
        t = raised(() -> sr.setSeed((byte[]) null));
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "SecureRandom.setSeed((byte[]) null) must throw NullPointerException, and"
                        + " BEFORE the receiver is examined (SecureRandom.java:724); got "
                        + nameOf(t));
        check(t.getMessage() == null, "and its message is NULL; got " + t.getMessage());
        t = raised(() -> new SecureRandom((byte[]) null));
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "new SecureRandom((byte[]) null) must throw NullPointerException — the"
                        + " CONSTRUCTOR is a third site with its own arity gate"
                        + " (SecureRandom.java:266); got " + nameOf(t));
        check(t.getMessage() == null, "and its message is NULL; got " + t.getMessage());

        // The negative half, without which every row above passes an implementation that
        // rejects EVERY array. An empty array is not a null one and must be accepted.
        check(nameOf(raised(() -> sr.nextBytes(new byte[0]))).equals("none"),
                "nextBytes(new byte[0]) must return normally — empty is not null");
        check(nameOf(raised(() -> sr.setSeed(new byte[0]))).equals("none"),
                "setSeed(new byte[0]) must return normally");
        check(nameOf(raised(() -> new SecureRandom(new byte[0]))).equals("none"),
                "new SecureRandom(new byte[0]) must return normally");

        // (2) RANGED INTEGERS. F12 §3.2 cleared this axis for the TYPE and found one
        // wrong WORDING. Integer.MIN_VALUE is the load-bearing column: a `<= 0` test
        // rejects it, an `abs(n) > ...` or a `n == -1` test does not.
        t = raised(() -> sr.generateSeed(-1));
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "generateSeed(-1) must throw IllegalArgumentException; got " + nameOf(t));
        check("numBytes cannot be negative".equals(t.getMessage()),
                "and the message is HotSpot's verbatim (SecureRandom.java:878) — this VM"
                        + " said \"numBytes must be non-negative\", which is the same"
                        + " sentence and a different string, and only a message row can see"
                        + " the difference; got " + t.getMessage());
        check("java.lang.IllegalArgumentException"
                        .equals(nameOf(raised(() -> sr.generateSeed(Integer.MIN_VALUE)))),
                "generateSeed(Integer.MIN_VALUE) must throw too — the column that separates"
                        + " a `<= 0` guard from one written around -1");
        check(sr.generateSeed(0).length == 0,
                "generateSeed(0) is the ACCEPTING boundary: an empty array, not a throw");
        // The STATIC twin. A separate registration, and the two have drifted before.
        t = raised(() -> SecureRandom.getSeed(-1));
        check("java.lang.IllegalArgumentException".equals(nameOf(t)),
                "the static SecureRandom.getSeed(-1) must throw IllegalArgumentException —"
                        + " a separate registration from the instance method above; got "
                        + nameOf(t));
        check("numBytes cannot be negative".equals(t.getMessage()),
                "with the same message as its instance twin; got " + t.getMessage());
        check(SecureRandom.getSeed(0).length == 0, "and getSeed(0) is an empty array");
        check("java.lang.IllegalArgumentException".equals(nameOf(raised(() -> sr.nextInt(0)))),
                "SecureRandom.nextInt(0) must throw IllegalArgumentException");
        check("java.lang.IllegalArgumentException"
                        .equals(nameOf(raised(() -> sr.nextInt(Integer.MIN_VALUE)))),
                "and nextInt(Integer.MIN_VALUE) too");

        // (3) THE SELECTOR STRING. Both of these used to be IllegalArgumentException, and
        // the first of them carried the RIGHT message under the WRONG class — which is
        // exactly the shape a message-only assertion cannot see.
        t = raised(() -> SecureRandom.getInstance(null));
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "SecureRandom.getInstance(null) must throw NullPointerException, not"
                        + " IllegalArgumentException: SecureRandom.java:391 opens with"
                        + " Objects.requireNonNull(algorithm, ...). This VM threw IAE with"
                        + " the SAME message, so the class is the whole assertion; got "
                        + nameOf(t));
        check("null algorithm name".equals(t.getMessage()),
                "and it keeps HotSpot's message across the class change; got "
                        + t.getMessage());
        t = raised(() -> SecureRandom.getInstance(""));
        check("java.security.NoSuchAlgorithmException".equals(nameOf(t)),
                "getInstance(\"\") must be NoSuchAlgorithmException — the empty name is not"
                        + " a null name, it is a name that no provider serves, and it must"
                        + " reach the ordinary dead end rather than an early-return guard;"
                        + " got " + nameOf(t));
        check(" SecureRandom not available".equals(t.getMessage()),
                "and the message is the ordinary template applied to the empty string, with"
                        + " its LEADING SPACE intact — the row that proves the empty name"
                        + " travelled the normal path instead of a special-cased one; got "
                        + t.getMessage());
        t = raised(() -> SecureRandom.getInstance("NO-SUCH-PRNG"));
        check("java.security.NoSuchAlgorithmException".equals(nameOf(t))
                        && "NO-SUCH-PRNG SecureRandom not available".equals(t.getMessage()),
                "getInstance(\"NO-SUCH-PRNG\") must refuse with that same template; got "
                        + nameOf(t) + " / " + t.getMessage());
        check(nameOf(raised(() -> SecureRandom.getInstance("sha1prng"))).equals("none"),
                "JCA lookup is CASE-INSENSITIVE: getInstance(\"sha1prng\") must resolve. A"
                        + " case-sensitive table passes every refusal row above and fails"
                        + " only here");

        // (3a) AND THE NAME IS CARRIED THROUGH VERBATIM. F25 §6.4 measured this and
        // deliberately did not assert it, because F12 §3.5 records that
        // secure_random_static_provider normalises to upper-case alphanumeric for the
        // LOOKUP and says nothing about what getAlgorithm() then reports. This lane was
        // assigned the row anyway: a red row here is a finding for
        // native-builtins/src/securerandom.rs, not a reason to drop it.
        //
        // The row is only half a discriminator on its own. Its other half is
        // getInstanceArgumentOrder()'s `"SHA1PRNG".equals(viaProvider.getAlgorithm())`,
        // which asks the SAME question of an upper-case request: together they say the
        // answer TRACKS the request, where either alone also passes an implementation
        // that hard-cases every answer in one direction.
        check("sha1prng".equals(SecureRandom.getInstance("sha1prng").getAlgorithm()),
                "getAlgorithm() reports the spelling that was ASKED FOR, not a"
                        + " normalised one — JCA lookup is case-insensitive and the name"
                        + " is carried through verbatim; measured \"sha1prng\" on"
                        + " jdk-25.0.3+9");
        // The two-argument overloads are SEPARATE registrations — that is this family's
        // founding lesson (§the getInstance argument order below) and this tree has a
        // recorded defect where exactly the Provider-object form behaved differently from
        // its String-named twin. A fix that threads the asked-for spelling through the
        // one-argument body only is invisible without these two.
        check("sha1prng".equals(
                        SecureRandom.getInstance("sha1prng", "SUN").getAlgorithm()),
                "getInstance(\"sha1prng\", \"SUN\").getAlgorithm() carries the asked-for"
                        + " spelling too — a separate registration from the one-argument"
                        + " form");
        check("sha1prng".equals(
                        SecureRandom.getInstance("sha1prng", Security.getProvider("SUN"))
                                .getAlgorithm()),
                "and so does the Provider-object overload — the third registration, and"
                        + " the one the recorded \"provider ignored\" defect was found on");

        getInstanceArgumentOrder();
        // The only family in this file that publishes its own size. It is a tripwire in
        // the sense sectionEnd() is elsewhere in the suite: a block that silently loses
        // rows to an edit still prints a CK line, and this one prints a DIFFERENT number.
        // The value is MEASURED on jdk-25.0.3+9, not counted on paper.
        int n = checks - mark;
        if (n != 52) {
            throw new AssertionError("srArgKinds ran " + n + " checks, header says 52");
        }
        System.out.println("CK RJdkSecurity srArgKinds=" + n);
    }

    /**
     * The argument ORDER of the three two-argument {@code getInstance} overloads.
     *
     * <p>F12 §2. This is the divergence no per-method audit reaches, because the method
     * rejects both of its arguments correctly — in the wrong order. All three overloads
     * open with {@code Objects.requireNonNull(algorithm, "null algorithm name")}
     * ({@code SecureRandom.java:439} for the {@code String} provider, {@code :481} for the
     * {@code Provider}), and the provider is examined only afterwards.
     * {@code native_secure_random_get_instance_with_provider} ran its provider check
     * first, so a null algorithm plus a bad provider answered the provider's complaint.
     *
     * <p>Fixing the one-argument form does not fix this: the one-argument body is reached
     * only after both provider checks have already had their chance to throw, so the null
     * check has to be hoisted into the two-argument body itself.
     *
     * <p>The wrong order came from a comment that is TRUE — "the provider is resolved
     * BEFORE the algorithm is looked up" ({@code jca/provider_chain.rs:2612-2614}) — read
     * one clause too far. The provider does precede the algorithm's LOOKUP. It does not
     * precede the algorithm's NULL CHECK, and only these rows can tell the two apart.
     */
    static void getInstanceArgumentOrder() throws Exception {
        java.security.Provider sun = Security.getProvider("SUN");
        check(sun != null, "the SUN provider is the premise of the rows below");

        // A null algorithm wins over EVERY provider argument, good or bad. Each row names
        // the answer a provider-first implementation gives instead.
        check("java.lang.NullPointerException"
                        .equals(nameOf(raised(() -> SecureRandom.getInstance(null, "SUN")))),
                "getInstance(null, \"SUN\") — a VALID provider — must still be NPE. This is"
                        + " the row that shows the null check is not merely reached, but"
                        + " reached FIRST: a correct provider costs two lookups before the"
                        + " null is noticed if the order is wrong");
        Throwable t = raised(() -> SecureRandom.getInstance(null, "NOPE"));
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "getInstance(null, \"NOPE\") — BOTH arguments bad — must be NPE. A"
                        + " provider-first body answers NoSuchProviderException here, and"
                        + " this cell exists in no single-argument probe; got " + nameOf(t));
        check("null algorithm name".equals(t.getMessage()),
                "and the message names the ALGORITHM, not the provider — a body that"
                        + " reported \"no such provider: NOPE\" through an NPE would pass"
                        + " the row above and fail this one; got " + t.getMessage());
        check("java.lang.NullPointerException".equals(
                        nameOf(raised(() -> SecureRandom.getInstance(null, (String) null)))),
                "getInstance(null, (String) null) must be NPE — a provider-first body"
                        + " answers IllegalArgumentException \"missing provider\"");
        check("java.lang.NullPointerException"
                        .equals(nameOf(raised(() -> SecureRandom.getInstance(null, "")))),
                "getInstance(null, \"\") must be NPE — the EMPTY provider name takes the"
                        + " same IllegalArgumentException arm as the null one, so this is a"
                        + " second, independently-reachable way to lose the ordering");
        check("java.lang.NullPointerException".equals(
                        nameOf(raised(() -> SecureRandom.getInstance(null, (Provider) null)))),
                "getInstance(null, (Provider) null) must be NPE — the THIRD overload is a"
                        + " SEPARATE registration and can be fixed independently of the two"
                        + " String-provider ones");
        check("java.lang.NullPointerException"
                        .equals(nameOf(raised(() -> SecureRandom.getInstance(null, sun)))),
                "getInstance(null, sunProvider) must be NPE too");

        // The CONTROL. Without these, every row above also passes an implementation that
        // answers NPE for anything it does not like: the provider complaints must still
        // be the provider's, with a GOOD algorithm.
        check("java.lang.IllegalArgumentException".equals(
                        nameOf(raised(() -> SecureRandom.getInstance("SHA1PRNG", (String) null)))),
                "getInstance(\"SHA1PRNG\", (String) null) must be IllegalArgumentException,"
                        + " NOT NullPointerException — the null PROVIDER is not the null"
                        + " ALGORITHM, and this row is what stops the fix from becoming"
                        + " \"throw NPE on any null\"");
        check("java.lang.IllegalArgumentException"
                        .equals(nameOf(raised(() -> SecureRandom.getInstance("SHA1PRNG", "")))),
                "getInstance(\"SHA1PRNG\", \"\") must be IllegalArgumentException");
        check("java.security.NoSuchProviderException"
                        .equals(nameOf(raised(() -> SecureRandom.getInstance("SHA1PRNG", "NOPE")))),
                "getInstance(\"SHA1PRNG\", \"NOPE\") must be NoSuchProviderException — an"
                        + " ABSENT provider is a third answer again, distinct from both"
                        + " nulls above");
        check("java.lang.IllegalArgumentException".equals(
                        nameOf(raised(() -> SecureRandom.getInstance("SHA1PRNG", (Provider) null)))),
                "getInstance(\"SHA1PRNG\", (Provider) null) must be IllegalArgumentException");
        check("java.security.NoSuchAlgorithmException"
                        .equals(nameOf(raised(() -> SecureRandom.getInstance("NOPE", "SUN")))),
                "getInstance(\"NOPE\", \"SUN\") must be NoSuchAlgorithmException — the"
                        + " algorithm is LOOKED UP after the provider is resolved, which is"
                        + " the true half of the comment that produced the defect");
        check(nameOf(raised(() -> SecureRandom.getInstance("SHA1PRNG", "SUN"))).equals("none"),
                "and the all-good combination must simply work");

        // The MESSAGES for the six provider-argument rows above. F25 §3.3 left every one
        // of them class-only, on the stated ground that CratonVM's strings had never been
        // measured; this lane was assigned them, so a red row here is a finding for
        // native-builtins/src/securerandom.rs and not a reason to drop the row.
        //
        // Measured on jdk-25.0.3+9. THREE distinct strings across SIX call sites, and the
        // split is the point: "missing provider" is raised by the ARGUMENT check before
        // any lookup happens, and the other two are raised BY the lookup. A body that
        // produced one apology for every provider complaint passes any single row here.
        t = raised(() -> SecureRandom.getInstance("SHA1PRNG", (String) null));
        check(t != null && "missing provider".equals(t.getMessage()),
                "getInstance(\"SHA1PRNG\", (String) null) says \"missing provider\" — the"
                        + " ARGUMENT check's string, raised before any provider is looked"
                        + " up; got " + t);
        t = raised(() -> SecureRandom.getInstance("SHA1PRNG", ""));
        check(t != null && "missing provider".equals(t.getMessage()),
                "and the EMPTY provider name takes the identical arm, string included —"
                        + " an implementation that special-cased \"\" would answer here and"
                        + " not above; got " + t);
        t = raised(() -> SecureRandom.getInstance("SHA1PRNG", (Provider) null));
        check(t != null && "missing provider".equals(t.getMessage()),
                "and the Provider-object overload's null arm carries the same string; got "
                        + t);
        t = raised(() -> SecureRandom.getInstance("SHA1PRNG", "NOPE"));
        check(t != null && "no such provider: NOPE".equals(t.getMessage()),
                "an ABSENT provider names ITSELF and not the algorithm — the string is how"
                        + " a caller tells a mistyped provider from a mistyped algorithm,"
                        + " and both refusals are otherwise a bare exception; got " + t);
        t = raised(() -> SecureRandom.getInstance("NOPE", "SUN"));
        check(t != null && "no such algorithm: NOPE for provider SUN".equals(t.getMessage()),
                "a present provider that does not serve the algorithm names BOTH, in that"
                        + " order — this is the lookup's own string, so it is the row that"
                        + " proves the refusal came from the lookup rather than from an"
                        + " earlier guard; got " + t);
        t = raised(() -> SecureRandom.getInstance("SHA1PRNG", "SunJCE"));
        check(t != null
                        && "no such algorithm: SHA1PRNG for provider SunJCE".equals(t.getMessage()),
                "and the SunJCE row — the one that catches a discarded provider argument —"
                        + " names the real algorithm and the real provider, so a body that"
                        + " refused with a canned string would pass its class row and fail"
                        + " this one; got " + t);

        // The provider must actually be HONOURED, not merely accepted. Elsewhere in this
        // tree a getInstance(alg, Provider) DISCARDED its provider argument and silently
        // served the default one; no refusal row above can see that, because SUN is
        // ALSO the default provider for SHA1PRNG, so "asked for SUN, got SUN" is true
        // either way. The discriminator is a provider that is INSTALLED and does NOT
        // serve the algorithm: SunJCE ships in every OpenJDK and publishes no
        // SecureRandom service at all, so a body that drops the argument answers with
        // SUN's SHA1PRNG here instead of refusing.
        check("java.security.NoSuchAlgorithmException"
                        .equals(nameOf(raised(() -> SecureRandom.getInstance("SHA1PRNG", "SunJCE")))),
                "getInstance(\"SHA1PRNG\", \"SunJCE\") must be NoSuchAlgorithmException —"
                        + " the algorithm exists and that provider does not serve it. A"
                        + " body that resolves the algorithm and ignores the provider"
                        + " returns an object here and passes every other row in this arm");
        check("java.security.NoSuchAlgorithmException".equals(nameOf(
                        raised(() -> SecureRandom.getInstance("SHA1PRNG",
                                Security.getProvider("SunJCE"))))),
                "and the Provider-object overload refuses identically — a SEPARATE"
                        + " registration, and the one the JCA \"provider ignored\" defect"
                        + " was found on");
        SecureRandom viaProvider = SecureRandom.getInstance("SHA1PRNG", sun);
        check(viaProvider.getProvider() != null
                        && "SUN".equals(viaProvider.getProvider().getName()),
                "getInstance(\"SHA1PRNG\", sunProvider).getProvider() must be non-null and"
                        + " named SUN. The recorded defect this row catches is not the"
                        + " ignored provider (the two SunJCE rows above are that) but the"
                        + " construction route that stamped `algorithm` and never"
                        + " `provider`, so getProvider() came back null");
        check("SHA1PRNG".equals(viaProvider.getAlgorithm()),
                "and its getAlgorithm() is the name that was asked for");
        SecureRandom viaName = SecureRandom.getInstance("SHA1PRNG", "SUN");
        check("SUN".equals(viaName.getProvider().getName()),
                "and the String-named overload resolves to the same provider; got "
                        + viaName.getProvider().getName());
    }

    static void signatures() throws Exception {
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048);
        KeyPair kp = kpg.generateKeyPair();
        check(kp.getPublic().getAlgorithm().equals("RSA"), "public key algorithm");
        check(kp.getPublic().getFormat().equals("X.509"), "public key format");
        check(kp.getPrivate().getFormat().equals("PKCS#8"), "private key format");

        byte[] msg = "sign-me".getBytes(java.nio.charset.StandardCharsets.UTF_8);
        Signature signer = Signature.getInstance("SHA256withRSA");
        signer.initSign(kp.getPrivate());
        signer.update(msg);
        byte[] sig = signer.sign();
        check(sig.length == 256, "RSA-2048 signature length: " + sig.length);

        Signature verifier = Signature.getInstance("SHA256withRSA");
        verifier.initVerify(kp.getPublic());
        verifier.update(msg);
        check(verifier.verify(sig), "a valid signature must verify");

        verifier.initVerify(kp.getPublic());
        verifier.update("sign-me-not".getBytes(java.nio.charset.StandardCharsets.UTF_8));
        check(!verifier.verify(sig), "a tampered message must NOT verify");

        // A corrupted signature must fail, one way or the other, but never verify.
        byte[] bad = sig.clone();
        bad[0] ^= 0x55;
        verifier.initVerify(kp.getPublic());
        verifier.update(msg);
        boolean verified;
        try {
            verified = verifier.verify(bad);
        } catch (SignatureException e) {
            verified = false;
        }
        check(!verified, "a corrupted signature must not verify");

        // Key encoding round-trip through KeyFactory.
        KeyFactory kf = KeyFactory.getInstance("RSA");
        java.security.PublicKey pub2 = kf.generatePublic(
                new X509EncodedKeySpec(kp.getPublic().getEncoded()));
        java.security.PrivateKey priv2 = kf.generatePrivate(
                new PKCS8EncodedKeySpec(kp.getPrivate().getEncoded()));
        check(pub2.equals(kp.getPublic()), "public key encode/decode round-trip");
        check(Arrays.equals(priv2.getEncoded(), kp.getPrivate().getEncoded()),
                "private key encode/decode round-trip");

        // The re-decoded key must still verify a signature made with the original.
        Signature v2 = Signature.getInstance("SHA256withRSA");
        v2.initVerify(pub2);
        v2.update(msg);
        check(v2.verify(sig), "a round-tripped public key must still verify");

        // AES-GCM with a fixed key and IV is a KAT and is safe to print.
        SecretKey key = new SecretKeySpec(new byte[16], "AES");
        byte[] iv = new byte[12];
        Cipher enc = Cipher.getInstance("AES/GCM/NoPadding");
        enc.init(Cipher.ENCRYPT_MODE, key, new GCMParameterSpec(128, iv));
        byte[] ct = enc.doFinal(new byte[16]);
        check(hex(ct).equals("0388dace60b6a392f328c2b971b2fe78"
                + "ab6e47d42cec13bdf53a67b21257bddf"), "AES-GCM KAT: " + hex(ct));
        Cipher dec = Cipher.getInstance("AES/GCM/NoPadding");
        dec.init(Cipher.DECRYPT_MODE, key, new GCMParameterSpec(128, iv));
        check(Arrays.equals(dec.doFinal(ct), new byte[16]), "AES-GCM round-trip");

        // PBKDF2 is a KAT too (RFC 6070-style, 1 iteration).
        SecretKeyFactory skf = SecretKeyFactory.getInstance("PBKDF2WithHmacSHA256");
        byte[] dk = skf.generateSecret(new PBEKeySpec("password".toCharArray(),
                "salt".getBytes(java.nio.charset.StandardCharsets.UTF_8), 1, 256)).getEncoded();
        check(dk.length == 32, "PBKDF2 output length");
        check(hex(dk).startsWith("120fb6cffcf8b32c"), "PBKDF2 KAT: " + hex(dk).substring(0, 16));

        // BigInteger modular arithmetic underpins all of the above.
        BigInteger p = new BigInteger("170141183460469231731687303715884105727");
        check(p.isProbablePrime(40), "2^127-1 must be prime");
        check(BigInteger.valueOf(3).modPow(BigInteger.valueOf(100), p).signum() > 0, "modPow");
        System.out.println("CK RJdkSecurity gcm=" + hex(ct)
                + " pbkdf2=" + hex(dk).substring(0, 16) + " verify=true");
    }

    static void tls() throws Exception {
        // The default context must exist and name a real protocol.
        SSLContext def = SSLContext.getDefault();
        check(def != null, "SSLContext.getDefault");
        check(def.getProtocol() != null, "default protocol");

        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(null, null, null);
        check(ctx.getProtocol().equals("TLS"), "TLS context protocol");
        check(ctx.getSocketFactory() != null, "socket factory");
        check(ctx.getServerSocketFactory() != null, "server socket factory");

        SSLEngine engine = ctx.createSSLEngine("localhost", 443);
        check(engine != null, "createSSLEngine");
        engine.setUseClientMode(true);
        check(engine.getUseClientMode(), "client mode");
        check(engine.getPeerHost().equals("localhost"), "peer host");
        check(engine.getPeerPort() == 443, "peer port");
        check(engine.getSupportedProtocols().length > 0, "supported protocols");
        check(engine.getSupportedCipherSuites().length > 0, "supported cipher suites");
        check(engine.getEnabledCipherSuites().length > 0, "enabled cipher suites");

        // Protocol NAMES are stable strings; the SET varies by JDK, so only
        // membership of the modern protocol is asserted and printed.
        List<String> protos = new ArrayList<>(Arrays.asList(engine.getSupportedProtocols()));
        Collections.sort(protos);
        check(protos.contains("TLSv1.2") || protos.contains("TLSv1.3"),
                "a modern TLS protocol must be supported: " + protos);
        String modern = protos.contains("TLSv1.3") ? "TLSv1.3" : "TLSv1.2";

        SSLParameters params = engine.getSSLParameters();
        check(params != null, "SSL parameters");
        params.setProtocols(new String[] { modern });
        engine.setSSLParameters(params);
        check(Arrays.asList(engine.getEnabledProtocols()).contains(modern),
                "enabled protocol after set");

        check(engine.getSession() != null, "pre-handshake session");
        check(engine.getHandshakeStatus() != null, "handshake status");

        boolean threw = false;
        try {
            SSLContext.getInstance("NO-SUCH-TLS");
        } catch (NoSuchAlgorithmException expected) {
            threw = true;
        }
        check(threw, "an unknown SSL protocol must raise NoSuchAlgorithmException");

        // THE SSLSession ATTRIBUTE MAP, BY ARGUMENT KIND. F25 §6.6 measured this and left
        // it unwritten because CratonVM's answers had never been measured; NOMINATION 4 of
        // that record is this block. It is asked of the ENGINE's pre-handshake session,
        // which is the session this vector already holds — the same contract on a
        // NEGOTIATED session is RSslLiveSession's `attrs` family, and the two are not
        // duplicates: this directory's recorded defect (E31-1 §2) is a slot whose meaning
        // depends on the session's WIDTH, so the null-session door and the live door can
        // and do diverge.
        //
        // The pair worth having is rows 2 and 7: putValue says "arguments can not be null"
        // and getValue says "argument can not be null". Two methods, two strings, ONE
        // LETTER apart. A shared constant is wrong in one of the two places and only a
        // message row can see it — this is the same shape as generateSeed's wording above,
        // where the class was already right and the string was the whole defect.
        javax.net.ssl.SSLSession sess = engine.getSession();
        Throwable at = raised(() -> sess.putValue(null, "v"));
        check("java.lang.IllegalArgumentException".equals(nameOf(at)),
                "SSLSession.putValue(null, \"v\") must throw IllegalArgumentException, NOT"
                        + " NullPointerException — JSSE validates rather than dereferences"
                        + " here; got " + nameOf(at));
        check("arguments can not be null".equals(at.getMessage()),
                "and the message is PLURAL — SSLSessionImpl.putValue guards both arguments"
                        + " with one string; got " + at.getMessage());
        at = raised(() -> sess.putValue("k", null));
        check("java.lang.IllegalArgumentException".equals(nameOf(at)),
                "a null VALUE is refused by the same guard as a null NAME — an"
                        + " implementation that checked only the name accepts this and"
                        + " stores a null; got " + nameOf(at));
        check("arguments can not be null".equals(at.getMessage()),
                "with the same plural message; got " + at.getMessage());
        at = raised(() -> sess.putValue(null, null));
        check("arguments can not be null".equals(at.getMessage()),
                "and both-null is not a third case; got " + at.getMessage());
        at = raised(() -> sess.getValue(null));
        check("java.lang.IllegalArgumentException".equals(nameOf(at)),
                "SSLSession.getValue(null) must throw IllegalArgumentException too — a"
                        + " lookup that simply missed would return null and tell the caller"
                        + " the attribute was absent; got " + nameOf(at));
        check("argument can not be null".equals(at.getMessage()),
                "and its message is SINGULAR — one argument, one noun. This is the row that"
                        + " a single shared constant fails; got " + at.getMessage());
        at = raised(() -> sess.removeValue(null));
        check("java.lang.IllegalArgumentException".equals(nameOf(at)),
                "and removeValue(null) is the third site with the same contract; got "
                        + nameOf(at));
        check("argument can not be null".equals(at.getMessage()),
                "carrying the SINGULAR string, like getValue and unlike putValue; got "
                        + at.getMessage());

        // The controls. Without them every row above also passes a map whose putValue
        // silently does nothing and whose getValue always answers null.
        check(nameOf(raised(() -> sess.putValue("cratonvm.f36", "v"))).equals("none"),
                "a well-formed putValue must return normally");
        check("v".equals(sess.getValue("cratonvm.f36")),
                "and the attribute must round-trip; got " + sess.getValue("cratonvm.f36"));
        check(sess.getValue("cratonvm.f36") != null
                        && "java.lang.String".equals(sess.getValue("cratonvm.f36")
                                .getClass().getName()),
                "and come back as the java.lang.String that went in, not as the map that"
                        + " holds it");
        check("[cratonvm.f36]".equals(Arrays.toString(sess.getValueNames())),
                "getValueNames must name it; got " + Arrays.toString(sess.getValueNames()));
        check(sess.getValue("cratonvm.no-such-attribute") == null,
                "an ABSENT name is null and not a refusal — the negative that separates"
                        + " \"validates its argument\" from \"throws on anything unfamiliar\"");
        check(nameOf(raised(() -> sess.removeValue("cratonvm.no-such-attribute"))).equals("none"),
                "and removing an absent name is a no-op, not a throw");
        check(nameOf(raised(() -> sess.removeValue("cratonvm.f36"))).equals("none"),
                "removeValue of a present name returns normally");
        check("[]".equals(Arrays.toString(sess.getValueNames())),
                "and the map is empty again — a removeValue that no-ops leaves the name"
                        + " here; got " + Arrays.toString(sess.getValueNames()));
        System.out.println("CK RJdkSecurity tls=" + modern
                + " engine=" + (engine.getUseClientMode() ? "client" : "server"));
    }

    static void providers() {
        // At least the SUN/SunJCE providers must be present, and the service
        // lookup must be case-insensitive per the JCA spec.
        List<String> names = new ArrayList<>();
        for (java.security.Provider p : Security.getProviders()) {
            names.add(p.getName());
        }
        check(names.contains("SUN"), "the SUN provider must be installed: " + names);
        check(Security.getProvider("SUN") != null, "getProvider(SUN)");
        check(Security.getProvider("cratonvm-no-such-provider") == null,
                "an unknown provider must be null, not fabricated");
        check(Security.getAlgorithms("MessageDigest").contains("SHA-256")
                || Security.getAlgorithms("MessageDigest").contains("SHA-256".toUpperCase(
                        Locale.ROOT)), "MessageDigest algorithms must include SHA-256");
        System.out.println("CK RJdkSecurity providerSun=true");
    }

    /**
     * Advertised versus served: every name the provider chain publishes must be
     * one the engine will hand over, and no name it refuses may be published.
     *
     * The three records this closes -- W4-3-security-getalgorithms-short-list.md,
     * W7-29-jca-advertise-implement-gaps.md, W7-63-jca-advertise-vs-serve.md --
     * had their fixes ratcheted only by Rust unit tests over the seed map and by
     * a probe under probes/, which regression-suite/run.sh never runs. Every
     * assertion here fails on the pre-fix behaviour:
     *
     *   MD2                 advertised by SUN and refused by getInstance
     *   SHAKE128-256/256-512 neither implemented nor advertised
     *   SHAKE128 / SHAKE256 the ALIAS half -- resolvable on HotSpot, refused
     *                       here even after the primaries landed, because
     *                       getInstance's only gate is the digest engine's own
     *                       name table and nothing on that path reads the
     *                       provider chain's alias rows
     *   Signature           getInstance accepted EVERY string and deferred the
     *                       failure to sign()/verify() as the wrong exception
     *   getAlgorithms       returned a plain mutable HashSet
     *
     * Every vector is HotSpot 25's own answer, so this section holds on the
     * oracle as well as on both CratonVM modes. Two knowing divergences are
     * deliberately NOT asserted here because they would fail on HotSpot: SUN's
     * KeyFactory no longer advertises the ML-DSA umbrella, and SunJCE's no
     * longer advertises ML-KEM. The loops below assert the invariant those
     * removals restore -- advertised implies serviceable -- which is true on
     * both VMs by different routes.
     */
    static void advertisedVersusServed() throws Exception {
        // MD2, RFC 1319. Advertised by SUN for three waves while getInstance
        // refused it; implemented rather than de-advertised, because SunRsaSign
        // and SunMSCAPI both advertise MD2withRSA, which resolves MD2
        // internally. These are RFC 1319's own vectors, re-measured on HotSpot.
        MessageDigest md2 = MessageDigest.getInstance("MD2");
        check(md2.getDigestLength() == 16, "MD2 digest length: " + md2.getDigestLength());
        String md2Empty = hex(md2.digest(new byte[0]));
        check(md2Empty.equals("8350e5a3e24c153df2275c9f80692773"), "MD2 of empty: " + md2Empty);
        String md2Abc = hex(md2.digest("abc".getBytes(java.nio.charset.StandardCharsets.UTF_8)));
        check(md2Abc.equals("da853b0d3f88d99b30283a69e6ded6bb"), "MD2(abc): " + md2Abc);

        // The two SHAKE XOFs read out to the fixed length their JDK name names.
        MessageDigest shake128 = MessageDigest.getInstance("SHAKE128-256");
        check(shake128.getDigestLength() == 32,
                "SHAKE128-256 length: " + shake128.getDigestLength());
        String s128 = hex(shake128.digest("abc".getBytes(
                java.nio.charset.StandardCharsets.UTF_8)));
        check(s128.equals("5881092dd818bf5cf8a3ddb793fbcba74097d5c526a6d35f97b83351940f2cc8"),
                "SHAKE128-256(abc): " + s128);
        MessageDigest shake256 = MessageDigest.getInstance("SHAKE256-512");
        check(shake256.getDigestLength() == 64,
                "SHAKE256-512 length: " + shake256.getDigestLength());
        String s256 = hex(shake256.digest("abc".getBytes(
                java.nio.charset.StandardCharsets.UTF_8)));
        check(s256.equals("483366601360a8771c6863080cc4114d8db44530f8f1e1ee4f94ea37e78b5739"
                        + "d5a15bef186a5386c75744c0527e1faa9f8726e462a12a4feb06bd8801e751e4"),
                "SHAKE256-512(abc): " + s256);

        // The alias half. Alg.Alias.MessageDigest.SHAKE128 = SHAKE128-256 on
        // HotSpot: the bare spelling resolves and produces byte-identical
        // output, while getAlgorithms lists only the hyphenated primary.
        String alias128 = hex(MessageDigest.getInstance("SHAKE128").digest(
                "abc".getBytes(java.nio.charset.StandardCharsets.UTF_8)));
        check(alias128.equals(s128), "SHAKE128 is an alias of SHAKE128-256: " + alias128);
        String alias256 = hex(MessageDigest.getInstance("SHAKE256").digest(
                "abc".getBytes(java.nio.charset.StandardCharsets.UTF_8)));
        check(alias256.equals(s256), "SHAKE256 is an alias of SHAKE256-512: " + alias256);

        java.util.Set<String> digestNames = Security.getAlgorithms("MessageDigest");
        check(digestNames.contains("MD2"), "MessageDigest algorithms must include MD2");
        check(digestNames.contains("SHAKE128-256") && digestNames.contains("SHAKE256-512"),
                "MessageDigest algorithms must include both SHAKE primaries: " + digestNames);
        check(!digestNames.contains("SHAKE128") && !digestNames.contains("SHAKE256"),
                "an ALIAS must not be advertised as an algorithm: " + digestNames);

        // Advertised implies serviceable, in both engines whose lists drifted.
        // One check each rather than one per name, so the count does not move
        // with the size of the provider's list.
        List<String> refusedDigests = new ArrayList<>();
        for (String algo : digestNames) {
            try {
                MessageDigest.getInstance(algo);
            } catch (NoSuchAlgorithmException refused) {
                refusedDigests.add(algo);
            }
        }
        check(refusedDigests.isEmpty(),
                "every advertised MessageDigest must be serviceable, refused: " + refusedDigests);

        List<String> refusedFactories = new ArrayList<>();
        for (String algo : Security.getAlgorithms("KeyFactory")) {
            try {
                KeyFactory.getInstance(algo);
            } catch (NoSuchAlgorithmException refused) {
                refusedFactories.add(algo);
            }
        }
        check(refusedFactories.isEmpty(),
                "every advertised KeyFactory must be serviceable, refused: " + refusedFactories);

        // Signature.getInstance used to answer EVERY string with an object
        // whose getAlgorithm() was "Unknown", so a caller probing with
        // catch (NoSuchAlgorithmException) concluded the algorithm was present
        // and met a SignatureException much later instead.
        for (String bogus : new String[] { "NO-SUCH-SIG", "AES", "HmacSHA256", "" }) {
            boolean threw = false;
            try {
                Signature.getInstance(bogus);
            } catch (NoSuchAlgorithmException expected) {
                threw = true;
            }
            check(threw, "Signature.getInstance(\"" + bogus
                    + "\") must raise NoSuchAlgorithmException");
        }

        // Last, because it mutates: the returned set is a view of platform
        // state, not the caller's to edit. HotSpot answers
        // Collections$UnmodifiableSet on every path including the empty ones.
        boolean unmodifiable = false;
        try {
            digestNames.add("CRATONVM-NOT-AN-ALGORITHM");
        } catch (UnsupportedOperationException expected) {
            unmodifiable = true;
        }
        check(unmodifiable, "Security.getAlgorithms must return an unmodifiable set");

        System.out.println("CK RJdkSecurity md2=" + md2Abc + " shake128=" + s128.substring(0, 16)
                + " digests=" + digestNames.size());
    }

    /**
     * `javax.net.ssl.trustStore` is how an application says "trust THIS and
     * nothing else", and `TrustManagerFactory.init(null)` is what has to obey
     * it: JSSE's default trust store is the platform roots only while the
     * property is unset, and the property REPLACES them rather than adding to
     * them.
     *
     * CratonVM ignored it, which failed in the widening direction — an
     * application that pinned its trust to one CA was given the whole public
     * root set (122 anchors where HotSpot reported 1) and still rejected the
     * one certificate it had asked to trust.
     *
     * Nothing here prints a certificate, a subject or a platform anchor COUNT:
     * the two VMs legitimately ship different root sets (118 vs 122), and the
     * borrowed anchor below is simply "the first RSA root this VM has", which
     * also differs. What is diffed is the shape the property produces — one
     * anchor, its own certificate accepted, an unrelated one refused.
     */
    static void defaultTrustStoreProperty() throws Exception {
        // Before touching the property: with none set, JSSE's default anchors
        // ARE `$JAVA_HOME/lib/security/cacerts`. Asserted as a RELATION rather
        // than a count, because the count is a property of whichever JDK image
        // the run uses — but "the default trust manager and cacerts hold the
        // same number of trusted certificates" holds on any of them, and it is
        // exactly what fails when the anchors come from the OS trust store
        // instead (measured: 122 from /etc/ssl/certs against cacerts' 118,
        // four of them CAs the JDK does not trust, one the host's own
        // self-signed machine certificate).
        System.out.println("CK RJdkSecurity defaultAnchorsAreCacerts=" + defaultAnchorsAreCacerts());
        checks++;

        String saved = System.getProperty("javax.net.ssl.trustStore");
        java.io.File f = java.io.File.createTempFile("rjdksec-trust", ".p12");
        try {
            java.security.cert.X509Certificate mine = null;
            java.security.cert.X509Certificate other = null;
            for (java.security.cert.X509Certificate c : platformAnchors()) {
                if (!"RSA".equals(c.getPublicKey().getAlgorithm())) {
                    continue;
                }
                if (mine == null) {
                    mine = c;
                } else if (!c.getSubjectX500Principal().equals(mine.getSubjectX500Principal())) {
                    other = c;
                    break;
                }
            }
            if (mine == null || other == null) {
                // Reported, never silently skipped: a run with no usable
                // platform anchors must not read as a pass of this stage.
                System.out.println("CK RJdkSecurity trustStoreProp=SKIPPED-no-rsa-anchors");
                checks++;
                return;
            }
            java.security.KeyStore ks =
                    java.security.KeyStore.getInstance(java.security.KeyStore.getDefaultType());
            ks.load(null, null);
            ks.setCertificateEntry("only", mine);
            try (java.io.OutputStream o = new java.io.FileOutputStream(f)) {
                ks.store(o, "changeit".toCharArray());
            }
            System.setProperty("javax.net.ssl.trustStore", f.getAbsolutePath());
            System.setProperty("javax.net.ssl.trustStorePassword", "changeit");

            javax.net.ssl.TrustManagerFactory tmf = javax.net.ssl.TrustManagerFactory
                    .getInstance(javax.net.ssl.TrustManagerFactory.getDefaultAlgorithm());
            tmf.init((java.security.KeyStore) null);
            javax.net.ssl.X509TrustManager x = null;
            for (javax.net.ssl.TrustManager tm : tmf.getTrustManagers()) {
                if (tm instanceof javax.net.ssl.X509TrustManager) {
                    x = (javax.net.ssl.X509TrustManager) tm;
                    break;
                }
            }
            if (x == null) {
                System.out.println("CK RJdkSecurity trustStoreProp=NO-X509-MANAGER");
                checks++;
                return;
            }
            java.security.cert.X509Certificate[] issuers = x.getAcceptedIssuers();
            System.out.println("CK RJdkSecurity trustStorePropAnchors="
                    + (issuers == null ? -1 : issuers.length) + " (expect 1)");
            checks++;
            System.out.println("CK RJdkSecurity trustStorePropOwnCert=" + verdict(x, mine));
            checks++;
            System.out.println("CK RJdkSecurity trustStorePropOtherCert=" + verdict(x, other));
            checks++;
        } finally {
            if (saved == null) {
                System.clearProperty("javax.net.ssl.trustStore");
                System.clearProperty("javax.net.ssl.trustStorePassword");
            } else {
                System.setProperty("javax.net.ssl.trustStore", saved);
            }
            f.delete();
        }
    }

    /**
     * "true" on any JDK image, "false" when the default anchors come from
     * somewhere other than cacerts. Answers "no-cacerts" rather than a verdict
     * when the image has no such file, so a run on a trimmed image is visibly
     * inconclusive instead of quietly passing.
     */
    static String defaultAnchorsAreCacerts() throws Exception {
        java.io.File cacerts =
                new java.io.File(System.getProperty("java.home"), "lib/security/cacerts");
        if (!cacerts.isFile()) {
            return "no-cacerts";
        }
        java.security.KeyStore ks = java.security.KeyStore.getInstance("JKS");
        try (java.io.InputStream in = new java.io.FileInputStream(cacerts)) {
            ks.load(in, null);
        }
        int trusted = 0;
        for (java.util.Enumeration<String> e = ks.aliases(); e.hasMoreElements();) {
            if (ks.isCertificateEntry(e.nextElement())) {
                trusted++;
            }
        }
        return String.valueOf(trusted > 0 && platformAnchors().size() == trusted);
    }

    static java.util.List<java.security.cert.X509Certificate> platformAnchors() throws Exception {
        javax.net.ssl.TrustManagerFactory tmf = javax.net.ssl.TrustManagerFactory
                .getInstance(javax.net.ssl.TrustManagerFactory.getDefaultAlgorithm());
        tmf.init((java.security.KeyStore) null);
        for (javax.net.ssl.TrustManager tm : tmf.getTrustManagers()) {
            if (tm instanceof javax.net.ssl.X509TrustManager) {
                java.security.cert.X509Certificate[] a =
                        ((javax.net.ssl.X509TrustManager) tm).getAcceptedIssuers();
                return a == null ? Collections.emptyList() : Arrays.asList(a);
            }
        }
        return Collections.emptyList();
    }

    static String verdict(javax.net.ssl.X509TrustManager x,
            java.security.cert.X509Certificate cert) {
        try {
            x.checkServerTrusted(new java.security.cert.X509Certificate[] { cert }, "RSA");
            return "ACCEPTED";
        } catch (Exception e) {
            return "REJECTED";
        }
    }

    public static void main(String[] args) throws Exception {
        digests();
        secureRandoms();
        secureRandomArgumentKinds();
        signatures();
        tls();
        defaultTrustStoreProperty();
        providers();
        advertisedVersusServed();
        System.out.println("CK RJdkSecurity checks=" + checks);
        System.out.println("PASS RJdkSecurity (" + checks + " checks)");
    }
}
