use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::time::{Duration, Instant};

#[test]
fn bound_socket_connects_without_losing_its_local_port() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let peer = listener.local_addr().unwrap();
    let acceptor = std::thread::spawn(move || listener.accept().unwrap());

    let requested = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let stream = bind(&requested).unwrap();
    let bound_port = stream.local_addr().unwrap().port();
    assert_ne!(bound_port, 0, "bind(0) must allocate an outbound port");

    let stream = match start_bound(stream, &peer).unwrap() {
        StartConnect::Connected(stream) => stream,
        StartConnect::DeferredFailure(_, error) => {
            panic!("bound connect unexpectedly failed: {error}")
        }
        StartConnect::InProgress(stream) => {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match poll(&stream) {
                    ConnectPoll::Connected => break stream,
                    ConnectPoll::Failed(e) => panic!("bound connect failed: {e}"),
                    ConnectPoll::Pending if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    ConnectPoll::Pending => panic!("bound connect timed out"),
                }
            }
        }
    };
    assert_eq!(stream.local_addr().unwrap().port(), bound_port);
    drop(stream);
    drop(acceptor.join().unwrap());
}
