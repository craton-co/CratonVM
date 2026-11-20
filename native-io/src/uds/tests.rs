use super::*;

#[test]
fn path_too_long_is_rejected() {
    let long = "x".repeat(UNIX_PATH_MAX);
    let err = match SockAddrUn::new(&long) {
        Ok(_) => panic!("an over-long sun_path must be rejected"),
        Err(e) => e,
    };
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn addr_round_trips_the_path() {
    let (addr, len) = SockAddrUn::new("/tmp/x.sock").expect("addr");
    assert_eq!(addr.family as i32, af_unix());
    assert_eq!(len as usize, 2 + "/tmp/x.sock".len() + 1);
    assert_eq!(addr.path_string(len), "/tmp/x.sock");
}

#[test]
fn unnamed_addr_decodes_to_empty() {
    let addr = SockAddrUn {
        family: af_unix() as u16,
        path: [0u8; UNIX_PATH_MAX],
    };
    assert_eq!(addr.path_string(2), "");
}

#[test]
fn round_trip_over_a_real_socket() {
    if !is_supported() {
        return;
    }
    let dir = std::env::temp_dir();
    let path = dir.join(format!("cratonvm-uds-test-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let path_str = path.to_string_lossy().into_owned();
    // A temp dir deep enough to overflow sun_path would make this test
    // meaningless rather than failing; skip instead.
    if path_str.len() >= UNIX_PATH_MAX {
        return;
    }
    let listener = UdsListener::bind(&path_str, 16).expect("bind");
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _peer) = listener.accept().expect("accept");
        let mut buf = [0u8; 4];
        stream.read_exact(&mut buf).expect("read");
        stream.write_all(b"pong").expect("write");
        buf
    });
    {
        use std::io::{Read, Write};
        let mut client = connect(&path_str).expect("connect");
        client.write_all(b"ping").expect("client write");
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).expect("client read");
        assert_eq!(&buf, b"pong");
    }
    assert_eq!(&server.join().expect("join"), b"ping");
    let _ = std::fs::remove_file(&path);
}
