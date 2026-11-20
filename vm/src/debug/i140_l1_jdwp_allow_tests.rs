use super::{jdwp_allow_admits, jdwp_peer_text, parse_jdwp_allow, JdwpAllow};
use std::net::IpAddr;

fn ip(text: &str) -> IpAddr {
    text.parse().expect("an address")
}

fn admits(spec: &str, peer: &str) -> bool {
    jdwp_allow_admits(&parse_jdwp_allow(spec).expect("a valid list"), ip(peer))
}

#[test]
fn i140_l1_the_lists_hotspot_accepts() {
    assert_eq!(parse_jdwp_allow("*"), Ok(JdwpAllow::All));
    assert!(!admits("127.0.0.2", "127.0.0.1"));
    assert!(admits("127.0.0.1", "127.0.0.1"));
    assert!(admits("127.0.0.0/8", "127.0.0.1"));
    assert!(admits("10.1.2.3+127.0.0.1", "127.0.0.1"));
    assert!(admits("127.0.0.1/32+10.0.0.0/8", "127.0.0.1"));
    assert!(admits("127.0.0.2/0", "127.0.0.1"));
    assert!(!admits("::1", "127.0.0.1"));
    assert!(admits("::ffff:127.0.0.1", "127.0.0.1"));
    assert!(admits("::1", "::1"));
    assert!(!admits("10.0.0.0/8", "11.0.0.1"));
}

#[test]
fn i140_l1_the_lists_hotspot_refuses_with_its_texts() {
    let ip_error = "invalid IP address in allow option";
    let mask_error = "invalid netmask in allow option";
    let refused = |spec: &str| parse_jdwp_allow(spec).unwrap_err();
    assert_eq!(refused("10.1.2.3;127.0.0.1"), ("10.1.2.3;127.0.0.1".to_string(), ip_error));
    assert_eq!(refused("bogus"), ("bogus".to_string(), ip_error));
    assert_eq!(refused("localhost"), ("localhost".to_string(), ip_error));
    assert_eq!(refused("127.1"), ("127.1".to_string(), ip_error));
    assert_eq!(refused("10.0.0.0/8+bogus"), ("bogus".to_string(), ip_error));
    assert_eq!(refused("127.0.0.1+"), (String::new(), ip_error));
    assert_eq!(refused("127.0.0.1/40"), ("40".to_string(), mask_error));
    assert_eq!(refused("127.0.0.1/"), (String::new(), mask_error));
    assert_eq!(refused("127.0.0.1/abc"), ("abc".to_string(), mask_error));
    assert_eq!(refused("::1/129"), ("129".to_string(), mask_error));
    assert_eq!(
        refused("*+127.0.0.1"),
        ("*+127.0.0.1".to_string(), "allow option '*' cannot be expanded")
    );
}

#[test]
fn i140_l1_a_refused_peer_is_named_as_the_transport_names_it() {
    assert_eq!(jdwp_peer_text(ip("127.0.0.1"), false), "127.0.0.1");
    assert_eq!(jdwp_peer_text(ip("127.0.0.1"), true), "::ffff:127.0.0.1");
}
