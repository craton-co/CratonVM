// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 123, lane L1 -- the flip rule probe for the
// `JdkStringGetBytes` compat family (alignment bit 95,
// `CRATONVM_COMPAT_JDK_STRING_GET_BYTES`, default off). Page:
// docs/internal/proposals/interpreter/i74-L1-proposal-route-to-own-bytecode-natives-become-family-rows-20261107.md.
//
// String.getBytes(Charset) / getBytes(String) / getBytes() over Latin-1 and
// UTF-16 coder strings, for UTF-8, ISO-8859-1, US-ASCII, UTF-16 (BOM),
// UTF-16LE/BE, windows-1252, Shift_JIS, EUC-JP, by Charset and by name
// (aliases included); unmappable characters; unpaired surrogates; an unknown
// charset name (UnsupportedEncodingException); a null Charset and a null
// name (NPE); a long mixed string by length and hash. Bytes print as hex.
//
// Must be SAME in all three arms:
//   javac -d out L1W123StringGetBytesCharsets.java
//   java -cp out L1W123StringGetBytesCharsets
//   cratonvm --jdk-only -cp out L1W123StringGetBytesCharsets
//   cratonvm --compatible -cp out L1W123StringGetBytesCharsets
//   CRATONVM_COMPAT_JDK_STRING_GET_BYTES=1 cratonvm --compatible -cp out L1W123StringGetBytesCharsets
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint identical, Windows 11)
// empty default=
// empty UTF-8 cs= name=same
// empty ISO-8859-1 cs= name=same
// empty US-ASCII cs= name=same
// empty UTF-16 cs= name=same
// empty UTF-16LE cs= name=same
// empty UTF-16BE cs= name=same
// empty windows-1252 cs= name=same
// empty Shift_JIS cs= name=same
// empty EUC-JP cs= name=same
// ascii default=48656c6c6f2c20576f726c642030313233
// ascii UTF-8 cs=48656c6c6f2c20576f726c642030313233 name=same
// ascii ISO-8859-1 cs=48656c6c6f2c20576f726c642030313233 name=same
// ascii US-ASCII cs=48656c6c6f2c20576f726c642030313233 name=same
// ascii UTF-16 cs=feff00480065006c006c006f002c00200057006f0072006c006400200030003100320033 name=same
// ascii UTF-16LE cs=480065006c006c006f002c00200057006f0072006c00640020003000310032003300 name=same
// ascii UTF-16BE cs=00480065006c006c006f002c00200057006f0072006c006400200030003100320033 name=same
// ascii windows-1252 cs=48656c6c6f2c20576f726c642030313233 name=same
// ascii Shift_JIS cs=48656c6c6f2c20576f726c642030313233 name=same
// ascii EUC-JP cs=48656c6c6f2c20576f726c642030313233 name=same
// latin1 default=636166c3a920c3bfc2a9c280c29f
// latin1 UTF-8 cs=636166c3a920c3bfc2a9c280c29f name=same
// latin1 ISO-8859-1 cs=636166e920ffa9809f name=same
// latin1 US-ASCII cs=6361663f203f3f3f3f name=same
// latin1 UTF-16 cs=feff00630061006600e9002000ff00a90080009f name=same
// latin1 UTF-16LE cs=630061006600e9002000ff00a90080009f00 name=same
// latin1 UTF-16BE cs=00630061006600e9002000ff00a90080009f name=same
// latin1 windows-1252 cs=636166e920ffa93f3f name=same
// latin1 Shift_JIS cs=6361663f203f3f3f3f name=same
// latin1 EUC-JP cs=6361668fabb1208fabf38fa2ed3f3f name=same
// euro default=707269636520e282ac203520e2809c6869e2809d20e284a2
// euro UTF-8 cs=707269636520e282ac203520e2809c6869e2809d20e284a2 name=same
// euro ISO-8859-1 cs=7072696365203f2035203f68693f203f name=same
// euro US-ASCII cs=7072696365203f2035203f68693f203f name=same
// euro UTF-16 cs=feff00700072006900630065002020ac002000350020201c00680069201d00202122 name=same
// euro UTF-16LE cs=700072006900630065002000ac202000350020001c20680069001d2020002221 name=same
// euro UTF-16BE cs=00700072006900630065002020ac002000350020201c00680069201d00202122 name=same
// euro windows-1252 cs=70726963652080203520936869942099 name=same
// euro Shift_JIS cs=7072696365203f203520816768698168203f name=same
// euro EUC-JP cs=7072696365203f203520a1c86869a1c9208fa2ef name=same
// japanese default=e697a5e69cace8aa9e20e381b2e38289e3818ce381aa20e382abe382bfe382abe3838a20efbdb1
// japanese UTF-8 cs=e697a5e69cace8aa9e20e381b2e38289e3818ce381aa20e382abe382bfe382abe3838a20efbdb1 name=same
// japanese ISO-8859-1 cs=3f3f3f203f3f3f3f203f3f3f3f203f name=same
// japanese US-ASCII cs=3f3f3f203f3f3f3f203f3f3f3f203f name=same
// japanese UTF-16 cs=feff65e5672c8a9e002030723089304c306a002030ab30bf30ab30ca0020ff71 name=same
// japanese UTF-16LE cs=e5652c679e8a2000723089304c306a302000ab30bf30ab30ca30200071ff name=same
// japanese UTF-16BE cs=65e5672c8a9e002030723089304c306a002030ab30bf30ab30ca0020ff71 name=same
// japanese windows-1252 cs=3f3f3f203f3f3f3f203f3f3f3f203f name=same
// japanese Shift_JIS cs=93fa967b8cea2082d082e782aa82c820834a835e834a836920b1 name=same
// japanese EUC-JP cs=c6fccbdcb8ec20a4d2a4e9a4aca4ca20a5aba5bfa5aba5ca208eb1 name=same
// mixed default=61c3a9e282ace697a5f09f98807a
// mixed UTF-8 cs=61c3a9e282ace697a5f09f98807a name=same
// mixed ISO-8859-1 cs=61e93f3f3f7a name=same
// mixed US-ASCII cs=613f3f3f3f7a name=same
// mixed UTF-16 cs=feff006100e920ac65e5d83dde00007a name=same
// mixed UTF-16LE cs=6100e900ac20e5653dd800de7a00 name=same
// mixed UTF-16BE cs=006100e920ac65e5d83dde00007a name=same
// mixed windows-1252 cs=61e9803f3f7a name=same
// mixed Shift_JIS cs=613f3f93fa3f7a name=same
// mixed EUC-JP cs=618fabb13fc6fc3f7a name=same
// lone-high default=783f79
// lone-high UTF-8 cs=783f79 name=same
// lone-high ISO-8859-1 cs=783f79 name=same
// lone-high US-ASCII cs=783f79 name=same
// lone-high UTF-16 cs=feff0078fffd0079 name=same
// lone-high UTF-16LE cs=7800fdff7900 name=same
// lone-high UTF-16BE cs=0078fffd0079 name=same
// lone-high windows-1252 cs=783f79 name=same
// lone-high Shift_JIS cs=783f79 name=same
// lone-high EUC-JP cs=783f79 name=same
// lone-low default=783f79
// lone-low UTF-8 cs=783f79 name=same
// lone-low ISO-8859-1 cs=783f79 name=same
// lone-low US-ASCII cs=783f79 name=same
// lone-low UTF-16 cs=feff0078fffd0079 name=same
// lone-low UTF-16LE cs=7800fdff7900 name=same
// lone-low UTF-16BE cs=0078fffd0079 name=same
// lone-low windows-1252 cs=783f79 name=same
// lone-low Shift_JIS cs=783f79 name=same
// lone-low EUC-JP cs=783f79 name=same
// high-at-end default=61623f
// high-at-end UTF-8 cs=61623f name=same
// high-at-end ISO-8859-1 cs=61623f name=same
// high-at-end US-ASCII cs=61623f name=same
// high-at-end UTF-16 cs=feff00610062fffd name=same
// high-at-end UTF-16LE cs=61006200fdff name=same
// high-at-end UTF-16BE cs=00610062fffd name=same
// high-at-end windows-1252 cs=61623f name=same
// high-at-end Shift_JIS cs=61623f name=same
// high-at-end EUC-JP cs=61623f name=same
// low-at-start default=3f6162
// low-at-start UTF-8 cs=3f6162 name=same
// low-at-start ISO-8859-1 cs=3f6162 name=same
// low-at-start US-ASCII cs=3f6162 name=same
// low-at-start UTF-16 cs=fefffffd00610062 name=same
// low-at-start UTF-16LE cs=fdff61006200 name=same
// low-at-start UTF-16BE cs=fffd00610062 name=same
// low-at-start windows-1252 cs=3f6162 name=same
// low-at-start Shift_JIS cs=3f6162 name=same
// low-at-start EUC-JP cs=3f6162 name=same
// reversed-pair default=3f3f
// reversed-pair UTF-8 cs=3f3f name=same
// reversed-pair ISO-8859-1 cs=3f3f name=same
// reversed-pair US-ASCII cs=3f3f name=same
// reversed-pair UTF-16 cs=fefffffdfffd name=same
// reversed-pair UTF-16LE cs=fdfffdff name=same
// reversed-pair UTF-16BE cs=fffdfffd name=same
// reversed-pair windows-1252 cs=3f3f name=same
// reversed-pair Shift_JIS cs=3f3f name=same
// reversed-pair EUC-JP cs=3f3f name=same
// nul-and-controls default=00017f0a
// nul-and-controls UTF-8 cs=00017f0a name=same
// nul-and-controls ISO-8859-1 cs=00017f0a name=same
// nul-and-controls US-ASCII cs=00017f0a name=same
// nul-and-controls UTF-16 cs=feff00000001007f000a name=same
// nul-and-controls UTF-16LE cs=000001007f000a00 name=same
// nul-and-controls UTF-16BE cs=00000001007f000a name=same
// nul-and-controls windows-1252 cs=00017f0a name=same
// nul-and-controls Shift_JIS cs=00017f0a name=same
// nul-and-controls EUC-JP cs=00017f0a name=same
// latin1-from-utf16 default=616263c3a9
// latin1-from-utf16 UTF-8 cs=616263c3a9 name=same
// latin1-from-utf16 ISO-8859-1 cs=616263e9 name=same
// latin1-from-utf16 US-ASCII cs=6162633f name=same
// latin1-from-utf16 UTF-16 cs=feff00610062006300e9 name=same
// latin1-from-utf16 UTF-16LE cs=610062006300e900 name=same
// latin1-from-utf16 UTF-16BE cs=00610062006300e9 name=same
// latin1-from-utf16 windows-1252 cs=616263e9 name=same
// latin1-from-utf16 Shift_JIS cs=6162633f name=same
// latin1-from-utf16 EUC-JP cs=6162638fabb1 name=same
// alias utf8=c3a9e282ace697a5
// alias latin1=e93f3f
// alias ascii=3f3f3f
// alias Cp1252=e9803f
// alias SJIS=3f3f93fa
// alias eucjp=8fabb13fc6fc
// alias UTF_16=feff00e920ac65e5
// std UTF_8=c3a9e282ace697a5
// std ISO_8859_1=e93f3f
// std US_ASCII=3f3f3f
// std UTF_16=feff00e920ac65e5
// unknown 'no-such-charset' -> java.io.UnsupportedEncodingException: [no-such-charset]
// unknown 'UTF-9' -> java.io.UnsupportedEncodingException: [UTF-9]
// unknown '' -> java.io.UnsupportedEncodingException: []
// unknown 'bad name!' -> java.io.UnsupportedEncodingException: [bad name!]
// null-cs -> java.lang.NullPointerException: [null]
// null-name -> java.lang.NullPointerException: [null]
// null-cs-empty -> java.lang.NullPointerException: [null]
// big UTF-8 len=4121 hash=-500274609
// big ISO-8859-1 len=4057 hash=-1638163728
// big US-ASCII len=4057 hash=-1777495364
// big UTF-16 len=8116 hash=-1063771816
// big UTF-16LE len=8114 hash=-1040544021
// big UTF-16BE len=8114 hash=-1730949097
// big windows-1252 len=4057 hash=-1638163728
// big Shift_JIS len=4068 hash=-1661461658
// big EUC-JP len=4152 hash=314495169
// done
// HOTSPOT_EXPECTED_END

import java.io.UnsupportedEncodingException;
import java.nio.charset.Charset;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

public class L1W123StringGetBytesCharsets {
    static final String[] NAMES = {
        "UTF-8", "ISO-8859-1", "US-ASCII", "UTF-16", "UTF-16LE", "UTF-16BE",
        "windows-1252", "Shift_JIS", "EUC-JP",
    };
    static final String[] ALIASES = { "utf8", "latin1", "ascii", "Cp1252", "SJIS", "eucjp", "UTF_16" };

    static final String[][] SAMPLES = {
        { "empty", "" },
        { "ascii", "Hello, World 0123" },
        { "latin1", "caf" + u(0xe9) + " " + u(0xff, 0xa9, 0x80, 0x9f) },
        { "euro", "price " + u(0x20ac) + " 5 " + u(0x201c) + "hi" + u(0x201d) + " " + u(0x2122) },
        { "japanese", u(0x65e5, 0x672c, 0x8a9e) + " " + u(0x3072, 0x3089, 0x304c, 0x306a) + " "
                + u(0x30ab, 0x30bf, 0x30ab, 0x30ca) + " " + u(0xff71) },
        { "mixed", "a" + u(0xe9, 0x20ac, 0x65e5, 0xd83d, 0xde00) + "z" },
        { "lone-high", "x\ud800y" },
        { "lone-low", "x\udc00y" },
        { "high-at-end", "ab\ud83d" },
        { "low-at-start", "\ude00ab" },
        { "reversed-pair", "\ude00\ud83d" },
        { "nul-and-controls", "\u0000\u0001\u007f\n" },
        // A Latin-1 coder string built by substring of a UTF-16 one.
        { "latin1-from-utf16", (u(0x65e5) + "abc" + u(0xe9)).substring(1) },
    };

    /** A string of the given UTF-16 code units (keeps this source ASCII). */
    static String u(int... units) {
        StringBuilder sb = new StringBuilder(units.length);
        for (int c : units) {
            sb.append((char) c);
        }
        return sb.toString();
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder(b.length * 2);
        for (byte x : b) {
            sb.append(Character.forDigit((x >> 4) & 0xf, 16)).append(Character.forDigit(x & 0xf, 16));
        }
        return sb.toString();
    }

    static String err(Throwable t) {
        return t.getClass().getName() + ": [" + t.getMessage() + "]";
    }

    public static void main(String[] args) throws Exception {
        for (String[] s : SAMPLES) {
            String label = s[0];
            String str = s[1];
            System.out.println(label + " default=" + hex(str.getBytes()));
            for (String name : NAMES) {
                Charset cs = Charset.forName(name);
                String byCs = hex(str.getBytes(cs));
                String byName = hex(str.getBytes(name));
                System.out.println(label + " " + name + " cs=" + byCs
                        + (byName.equals(byCs) ? " name=same" : " name=" + byName));
            }
        }
        // Aliases go through the name lookup.
        String alias = u(0xe9, 0x20ac, 0x65e5);
        for (String a : ALIASES) {
            try {
                System.out.println("alias " + a + "=" + hex(alias.getBytes(a)));
            } catch (Throwable t) {
                System.out.println("alias " + a + " -> " + err(t));
            }
        }
        // The StandardCharsets constants (the front's identity fast path).
        System.out.println("std UTF_8=" + hex(alias.getBytes(StandardCharsets.UTF_8)));
        System.out.println("std ISO_8859_1=" + hex(alias.getBytes(StandardCharsets.ISO_8859_1)));
        System.out.println("std US_ASCII=" + hex(alias.getBytes(StandardCharsets.US_ASCII)));
        System.out.println("std UTF_16=" + hex(alias.getBytes(StandardCharsets.UTF_16)));

        // Unknown names.
        for (String bad : new String[] { "no-such-charset", "UTF-9", "", "bad name!" }) {
            try {
                System.out.println("unknown '" + bad + "'=" + hex("abc".getBytes(bad)));
            } catch (UnsupportedEncodingException e) {
                System.out.println("unknown '" + bad + "' -> " + err(e));
            } catch (Throwable t) {
                System.out.println("unknown '" + bad + "' -> other " + err(t));
            }
        }
        // Null charset / name.
        try {
            System.out.println("null-cs=" + hex("abc".getBytes((Charset) null)));
        } catch (Throwable t) {
            System.out.println("null-cs -> " + err(t));
        }
        try {
            System.out.println("null-name=" + hex("abc".getBytes((String) null)));
        } catch (Throwable t) {
            System.out.println("null-name -> " + err(t));
        }
        try {
            System.out.println("null-cs-empty=" + hex("".getBytes((Charset) null)));
        } catch (Throwable t) {
            System.out.println("null-cs-empty -> " + err(t));
        }

        // A long string (past any small-buffer path), by length and hash.
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 4000; i++) {
            sb.append((char) ('a' + i % 26));
            if (i % 97 == 0) sb.append((char) 0xe9);
            if (i % 389 == 0) sb.append((char) 0x65e5);
            if (i % 1201 == 0) sb.append('\ud800');
        }
        String big = sb.toString();
        for (String name : NAMES) {
            byte[] b = big.getBytes(Charset.forName(name));
            System.out.println("big " + name + " len=" + b.length + " hash=" + Arrays.hashCode(b));
        }
        System.out.println("done");
    }
}
