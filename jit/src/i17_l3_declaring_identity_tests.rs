use super::*;

const DESC: &str = "(I)I";

fn with_stack<R>(keys: &[JitCompileMethodKey], body: impl FnOnce() -> R) -> R {
    JIT_COMPILE_STACK.with(|stack| stack.borrow_mut().extend(keys.iter().cloned()));
    let out = body();
    JIT_COMPILE_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        let keep = stack.len() - keys.len();
        stack.truncate(keep);
    });
    out
}

/// `invokestatic i17/Sub.f` (cp #7) resolves to `i17/A.f`. With the VM's
/// declaring-class id, a cycle through loader one's `A.f` (id 21) keeps
/// the call on dispatch only when the site's own loader resolves `A` to
/// that class: another loader's same-named `A` (id 22) is neither closed
/// nor kept. Without an id (no resolver, a `None` or `0` answer) the ask
/// matches by names, the pre-wave-17 answer.
#[test]
fn an_inherited_name_call_is_matched_by_its_declaring_class_identity() {
    let one = cratonvm_types::ClassId::new(21);
    let vm = JitVerdictRegistry::default();
    let declaring_name = |cp_idx: u16| (cp_idx == 7).then(|| "i17/A".to_string());
    let names: &dyn Fn(u16) -> Option<String> = &declaring_name;
    let this_loader = |cp_idx: u16| (cp_idx == 7).then_some(21u32);
    let other_loader = |cp_idx: u16| (cp_idx == 7).then_some(22u32);
    let sentinel = |_: u16| Some(0u32);
    let this_loader: &dyn Fn(u16) -> Option<u32> = &this_loader;
    let other_loader: &dyn Fn(u16) -> Option<u32> = &other_loader;
    let sentinel: &dyn Fn(u16) -> Option<u32> = &sentinel;

    let stack = [
        JitCompileMethodKey::of_class(one, "i17/A", "f", DESC),
        JitCompileMethodKey::of_class(one, "i17/A", "g", DESC),
    ];
    let (other_closed, closed) = with_stack(&stack, || {
        let cycle =
            cycle_site_declaring_class(&vm, Some(names), 7, "i17/Sub", "f", DESC, || true);
        assert_eq!(cycle.as_deref(), Some("i17/A"));
        // Another loader's `A.f` is not the open compile: nothing closes.
        let other = vm.note_recursive_compile_cycle("i17/A", "f", DESC, true, || {
            cycle_site_declaring_class_id(Some(other_loader), 7)
        });
        let same = vm.note_recursive_compile_cycle("i17/A", "f", DESC, true, || {
            cycle_site_declaring_class_id(Some(this_loader), 7)
        });
        (other, same)
    });
    assert!(
        !other_closed,
        "another loader's same-named A.f closes no cycle"
    );
    assert!(closed, "the site's own A.f closes the cycle");

    let keeps = |resolver: Option<&dyn Fn(u16) -> Option<u32>>| {
        vm.direct_call_requires_dispatch_of("i17/A", "f", DESC, || {
            cycle_site_declaring_class_id(resolver, 7)
        })
    };
    assert!(keeps(Some(this_loader)));
    assert!(
        !keeps(Some(other_loader)),
        "another loader's same-named A.f is not kept on dispatch"
    );
    // No identity: the conservative, name-matched answer.
    assert!(keeps(None));
    assert!(keeps(Some(sentinel)));
    assert_eq!(cycle_site_declaring_class_id(Some(this_loader), 8), None);
}
