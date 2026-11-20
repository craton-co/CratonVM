use super::*;

const CHILD: &str = "CRATONVM_FDLIMIT_TEST_CHILD";

fn nofile() -> libc::rlimit {
    // SAFETY: all-zero is a valid `rlimit`; `getrlimit` writes only into it.
    let mut l: libc::rlimit = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) }, 0);
    l
}

/// HotSpot's `-XX:+MaxFDLimit` applies to every mode: a default
/// (`Compatible`) VM started under a 1024-style soft limit must end with the
/// soft limit at the hard one. Runs in a child process so lowering the
/// limit cannot starve the other tests of this process.
#[test]
fn a_compatible_vm_raises_the_soft_nofile_limit_to_the_hard_limit() {
    if cratonvm_types::flags::runtime_var_os(CHILD).is_some() {
        let mut l = nofile();
        l.rlim_cur = l.rlim_cur.min(256);
        // SAFETY: setrlimit on our own process with a valid `rlimit`.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &l) }, 0);
        let _vm = Vm::new(crate::config::VmConfig::default());
        let after = nofile();
        assert_eq!(after.rlim_cur, after.rlim_max, "soft limit left below the hard limit");
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["a_compatible_vm_raises_the_soft_nofile_limit", "--test-threads=1"])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(status.success(), "child run failed: {status}");
}
