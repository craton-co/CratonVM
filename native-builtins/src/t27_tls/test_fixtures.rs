#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
pub const CA_CRT_PEM: &str = include_str!("../t27_certs/ca.crt");
pub const SERVER_CRT_PEM: &str = include_str!("../t27_certs/server.crt");
pub const SERVER_KEY_PEM: &str = include_str!("../t27_certs/server.key");
pub const SERVER1_CRT_PEM: &str = include_str!("../t27_certs/server1.crt");
pub const SERVER1_KEY_PEM: &str = include_str!("../t27_certs/server1.key");
pub const SERVER2_CRT_PEM: &str = include_str!("../t27_certs/server2.crt");
pub const SERVER2_KEY_PEM: &str = include_str!("../t27_certs/server2.key");
pub const CLIENT_CRT_PEM: &str = include_str!("../t27_certs/client.crt");
pub const CLIENT_KEY_PEM: &str = include_str!("../t27_certs/client.key");
