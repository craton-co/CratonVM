import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.Base64;

/** Verifies the unpadded base64url form required for an SD-JWT disclosure digest. */
public final class SdJwtDisclosureDigestProbe {
    public static void main(String[] args) throws Exception {
        byte[] disclosure = "WyJzYWx0IiwibmFtZSIsIkFsaWNlIl0".getBytes(StandardCharsets.UTF_8);
        byte[] digest = MessageDigest.getInstance("SHA-256").digest(disclosure);
        String encoded = Base64.getUrlEncoder().withoutPadding().encodeToString(digest);

        if (encoded.length() != 43 || encoded.indexOf('=') >= 0) {
            throw new AssertionError("expected 43-character unpadded base64url digest, got " + encoded);
        }
        System.out.println("sdjwtDisclosureDigest=" + encoded + " length=" + encoded.length());
    }
}
