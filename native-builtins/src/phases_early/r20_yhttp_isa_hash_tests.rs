use super::{p52_java_string_hash, p52_jdk_inet_address_hash};

/// The JDK formulas: v4 is the big-endian `int`; v6 sums signed-byte
/// groups; brackets and a scope do not count; non-addresses hash 0.
#[test]
fn the_address_hash_is_the_jdk_inet_address_hash() {
    assert_eq!(p52_jdk_inet_address_hash("127.0.0.1"), 2_130_706_433);
    assert_eq!(p52_jdk_inet_address_hash("255.255.255.255"), -1);
    assert_eq!(p52_jdk_inet_address_hash("::1"), 1);
    // fe 80 00 00 | 0 | 0 | 00 00 00 01, signed bytes: (-2 * 256 - 128) << 16, plus 1.
    assert_eq!(p52_jdk_inet_address_hash("fe80::1"), -41_943_039);
    assert_eq!(p52_jdk_inet_address_hash("fe80::1%3"), -41_943_039);
    assert_eq!(p52_jdk_inet_address_hash("[::1]"), 1);
    assert_eq!(p52_jdk_inet_address_hash(""), 0);
    // `new InetSocketAddress("127.0.0.1", 8080).hashCode()` on HotSpot 25.
    assert_eq!(
        p52_jdk_inet_address_hash("127.0.0.1").wrapping_add(8080),
        2_130_714_513
    );
}

#[test]
fn the_hostname_hash_is_string_hash_code() {
    assert_eq!(p52_java_string_hash(""), 0);
    assert_eq!(p52_java_string_hash("abc"), 96_354);
    assert_eq!(p52_java_string_hash("localhost"), -1_204_607_085);
}
