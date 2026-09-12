import java.util.*;
import javax.security.auth.x500.X500Principal;

/** L6 — `javax.security.auth.x500.X500Principal` (10 owning rows) plus the
 *  `java.security.Principal` contract it implements.
 *
 *  This is the second pure-parsing family in the lane after `java.net.URI`, and
 *  it earns the same treatment: RFC 2253 / RFC 1779 fix the grammar exactly,
 *  the JDK implements it exactly, and a hand-written native approximates it.
 *  No provider lookup, no key material, no I/O — every row is a string in and a
 *  string, boolean or exception out.
 *
 *  The shapes where implementations diverge, and each is asked here:
 *
 *    * the escape rules — `\,` `\+` `\"` `\\` `\#` `\;` `\<` `\>` and a leading
 *      or trailing space, which must be escaped and must round-trip;
 *    * hex-pair escapes (`\41`) and the `#` hex-string form of an attribute
 *      value, which is the DER-encoded spelling;
 *    * multi-valued RDNs joined with `+`, whose component ORDER is not
 *      significant to equality but is to `getName`;
 *    * the three output formats — RFC 2253 (the canonical spelling), RFC 1779
 *      (which uses different keywords and separators) and CANONICAL (which
 *      folds case and normalises whitespace);
 *    * the keyword table, and what happens to an OID with no keyword;
 *    * `equals`, which is defined on the CANONICAL form, so it folds case in
 *      the value and collapses internal whitespace.
 *
 *  `getEncoded` is printed as HEX, not as a byte array's `toString`, because
 *  the latter is an identity hash.
 */
public class L6X500Sweep {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    static final char[] HEX = "0123456789abcdef".toCharArray();

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
        }
    }

    static String hex(byte[] a) {
        if (a == null) return "null";
        StringBuilder sb = new StringBuilder();
        for (byte b : a) sb.append(HEX[(b >> 4) & 15]).append(HEX[b & 15]);
        return sb.toString();
    }

    /** Every accessor of one DN, so a fix to one that breaks another shows. */
    static void dump(String tag, String dn) {
        tv(tag + " ctor", () -> new X500Principal(dn).getName());
        tv(tag + " rfc2253", () -> new X500Principal(dn).getName(X500Principal.RFC2253));
        tv(tag + " rfc1779", () -> new X500Principal(dn).getName(X500Principal.RFC1779));
        tv(tag + " canonical", () -> new X500Principal(dn).getName(X500Principal.CANONICAL));
        tv(tag + " toString", () -> new X500Principal(dn).toString());
        tv(tag + " hashCode", () -> new X500Principal(dn).hashCode());
        tv(tag + " encoded", () -> hex(new X500Principal(dn).getEncoded()));
        tv(tag + " reparse encoded", () -> new X500Principal(new X500Principal(dn).getEncoded()).getName());
        tv(tag + " roundtrip rfc2253", () ->
            new X500Principal(new X500Principal(dn).getName(X500Principal.RFC2253))
                .equals(new X500Principal(dn)));
    }

    public static void main(String[] args) {
        // ---- the ordinary shapes, then the ones with structure
        dump("simple", "CN=Alice");
        dump("multiRdn", "CN=Alice,OU=Eng,O=Example,C=US");
        dump("spaces", "CN=Alice Smith, OU=Eng, O=Example Inc., C=US");
        dump("multiValued", "CN=Alice+OU=Eng,O=Example");
        dump("multiValuedReordered", "OU=Eng+CN=Alice,O=Example");
        dump("emptyValue", "CN=,O=Example");
        dump("oidKeyword", "1.2.840.113549.1.9.1=alice@example.com,CN=Alice");
        dump("unknownOid", "1.3.6.1.4.1.99999.1=x,CN=Alice");
        dump("hexValue", "CN=#04024869");
        dump("escapedComma", "CN=Smith\\, Alice,O=Example");
        dump("escapedPlus", "CN=a\\+b");
        dump("escapedQuote", "CN=a\\\"b");
        dump("escapedBackslash", "CN=a\\\\b");
        dump("escapedHash", "CN=\\#notHex");
        dump("escapedSemicolon", "CN=a\\;b");
        dump("escapedAngle", "CN=a\\<b\\>c");
        dump("leadingSpace", "CN=\\ Alice");
        dump("trailingSpace", "CN=Alice\\ ");
        dump("hexPairEscape", "CN=\\41lice");
        dump("nonAscii", "CN=Ren\u00e9");
        dump("utf8HexPair", "CN=Ren\\C3\\A9");
        dump("quotedValue", "CN=\"Smith, Alice\",O=Example");
        dump("semicolonSeparator", "CN=Alice;O=Example");
        dump("mixedCaseKeyword", "cn=Alice,ou=Eng");
        dump("dcComponents", "DC=example,DC=com");
        dump("street", "STREET=1 Main St,CN=Alice");
        dump("uid", "UID=alice,DC=example");
        dump("serialNumber", "SERIALNUMBER=12345,CN=Alice");
        dump("t", "T=Dr,CN=Alice");
        dump("givenName", "GIVENNAME=Alice,SURNAME=Smith");
        dump("initials", "INITIALS=A.S.,CN=Alice");
        dump("generation", "GENERATION=Jr,CN=Alice");
        dump("dnQualifier", "DNQUALIFIER=q,CN=Alice");
        dump("emptyDn", "");
        dump("internalWhitespace", "CN=a    b");
        dump("tabInValue", "CN=a\tb");

        // ---- the malformed forms: which throw, and with what message
        for (String dn : new String[] {
                "CN", "CN=", "=Alice", "CN=Alice,", ",CN=Alice", "CN=Alice,,O=x",
                "CN=Alice+", "+CN=Alice", "CN=a\\", "CN=#0", "CN=#zz",
                "CN=#0402", "NoSuchKeyword=x", "1.2.3.=x", "1..2=x",
                "CN=\"unterminated", "CN=a\\q", "CN=Alice,CN", "  ",
                "CN=Alice O=x"})
            tv("bad " + esc(dn), () -> new X500Principal(dn).getName());
        tv("null string", () -> new X500Principal((String) null).getName());
        tv("null bytes", () -> new X500Principal((byte[]) null).getName());
        tv("empty bytes", () -> new X500Principal(new byte[0]).getName());
        tv("garbage bytes", () -> new X500Principal(new byte[] {1, 2, 3, 4}).getName());
        tv("null stream", () -> new X500Principal((java.io.InputStream) null).getName());

        // ---- the two-argument constructor, which takes an OID->keyword map.
        // It is the tenth of this class's ten registrations and the only one
        // no other row reaches, so without these it would be the one triple
        // the retirement could not claim to have observed.
        tv("ctor+map known oid", () -> new X500Principal(
                "MYOID=x,CN=Alice",
                Collections.singletonMap("1.3.6.1.4.1.99999.1", "MYOID")).getName());
        tv("ctor+map roundtrips to oid", () -> new X500Principal(
                "MYOID=x",
                Collections.singletonMap("1.3.6.1.4.1.99999.1", "MYOID"))
                .getName(X500Principal.RFC2253));
        tv("ctor+map empty", () -> new X500Principal(
                "CN=Alice", Collections.<String, String>emptyMap()).getName());
        tv("ctor+map null map", () -> new X500Principal("CN=Alice", null).getName());
        tv("ctor+map unknown keyword", () -> new X500Principal(
                "NOPE=x", Collections.singletonMap("1.2.3", "MYOID")).getName());
        tv("ctor+map bad oid key", () -> new X500Principal(
                "MYOID=x", Collections.singletonMap("not-an-oid", "MYOID")).getName());
        tv("ctor+map null dn", () -> new X500Principal(
                (String) null, Collections.<String, String>emptyMap()).getName());

        // ---- getName's own argument contract
        tv("getName null format", () -> new X500Principal("CN=a").getName(null));
        tv("getName unknown format", () -> new X500Principal("CN=a").getName("NoSuchFormat"));
        tv("getName lowercase format", () -> new X500Principal("CN=a").getName("rfc2253"));
        tv("getName with oid map", () -> new X500Principal("1.3.6.1.4.1.99999.1=x")
                .getName(X500Principal.RFC2253, Collections.singletonMap("1.3.6.1.4.1.99999.1", "MYOID")));
        tv("getName with empty oid map", () -> new X500Principal("1.3.6.1.4.1.99999.1=x")
                .getName(X500Principal.RFC2253, Collections.<String, String>emptyMap()));
        tv("getName canonical rejects oid map", () -> new X500Principal("CN=a")
                .getName(X500Principal.CANONICAL, Collections.singletonMap("1.2.3", "X")));
        tv("getName null oid map", () -> new X500Principal("CN=a").getName(X500Principal.RFC2253, null));
        tv("getName oid map bad keyword", () -> new X500Principal("1.3.6.1.4.1.99999.1=x")
                .getName(X500Principal.RFC2253, Collections.singletonMap("1.3.6.1.4.1.99999.1", "1bad")));

        // ---- equality is defined on the CANONICAL form
        String[][] pairs = {
            {"CN=Alice", "CN=alice"},
            {"CN=Alice", "CN=Alice "},
            {"CN=Alice", "CN= Alice"},
            {"CN=a  b", "CN=a b"},
            {"CN=Alice,O=X", "cn=Alice,o=X"},
            {"CN=Alice+OU=Eng", "OU=Eng+CN=Alice"},
            {"CN=Alice,OU=Eng", "OU=Eng,CN=Alice"},
            {"CN=Alice", "CN=Alice,O=X"},
            {"CN=#04024869", "CN=#04024869"},
            {"DC=Example,DC=COM", "dc=example,dc=com"},
        };
        for (String[] q : pairs) {
            tv("equals " + q[0] + " | " + q[1],
               () -> new X500Principal(q[0]).equals(new X500Principal(q[1])));
            tv("hashEq " + q[0] + " | " + q[1],
               () -> new X500Principal(q[0]).hashCode() == new X500Principal(q[1]).hashCode());
            tv("canonEq " + q[0] + " | " + q[1],
               () -> new X500Principal(q[0]).getName(X500Principal.CANONICAL)
                       .equals(new X500Principal(q[1]).getName(X500Principal.CANONICAL)));
        }
        tv("equals null", () -> new X500Principal("CN=a").equals(null));
        tv("equals other type", () -> new X500Principal("CN=a").equals("CN=a"));
        tv("is a Principal", () -> new X500Principal("CN=a") instanceof java.security.Principal);
        tv("getName is Principal.getName", () -> {
            java.security.Principal q = new X500Principal("CN=Alice,O=X");
            return q.getName();
        });
        tv("encoded is stable", () -> {
            X500Principal q = new X500Principal("CN=Alice");
            return Arrays.equals(q.getEncoded(), q.getEncoded());
        });
        tv("getEncoded returns a copy", () -> {
            X500Principal q = new X500Principal("CN=Alice");
            byte[] b = q.getEncoded();
            b[0] = (byte) 0xff;
            return hex(q.getEncoded());
        });
        tv("class name", () -> new X500Principal("CN=a").getClass().getName());

        System.out.println("rows " + rows);
        System.out.println("DONE L6X500Sweep");
    }
}
