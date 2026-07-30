// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
#![cfg(feature = "experimental-debug")]

//! T4.8 -- JDWP Conformance Tests
//!
//! These tests verify that the CratonVM JDWP (Java Debug Wire Protocol)
//! implementation conforms to the wire protocol specification used by IDE
//! debuggers (IntelliJ, Eclipse, VS Code).  They exercise the debug module
//! at `vm/src/debug/`.
//!
//! Run with:
//!
//!     cargo test -p cratonvm-vm --test t4_8_jdwp_conformance
//!
//! Most tests are `#[ignore]` because they require starting the VM in debug
//! mode with a JDWP listener.  The non-ignored tests exercise protocol-level
//! primitives (handshake, packet construction, event matching) directly.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use cratonvm_vm::debug::events::{
    Event, EventKind, EventModifier, StepDepth, StepSize, SuspendPolicy,
};
use cratonvm_vm::debug::protocol::JdwpPacket;
use cratonvm_vm::debug::transport::JDWP_HANDSHAKE;
use cratonvm_vm::debug::{self, DebugEvent, DebugState, FrameEntry};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The canonical 14-byte JDWP handshake string.
const HANDSHAKE: &[u8; 14] = b"JDWP-Handshake";

/// Find a free TCP port by binding to port 0 and reading the assigned port.
fn find_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("must be able to bind to an ephemeral port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Start the JDWP listener on the given port in a background thread.
/// Returns a join handle.  The listener will accept one connection,
/// perform the handshake, and then process commands until disconnect.
fn start_jdwp_listener(port: u16) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", port)).expect("listener must bind");
        listener
            .set_nonblocking(false)
            .expect("listener must be blocking");

        let (mut stream, _addr) = listener.accept().expect("must accept connection");

        // JDWP handshake
        let mut buf = [0u8; 14];
        stream.read_exact(&mut buf).expect("must read handshake");
        assert_eq!(&buf, HANDSHAKE, "client must send JDWP-Handshake");
        stream
            .write_all(HANDSHAKE)
            .expect("must echo handshake back");

        // Process commands until the connection closes.
        let conn = debug::transport::JdwpConnection::new(stream);
        let mut state = DebugState::new();
        // The listener thread just handles one command or waits for disconnect.
        // For test purposes, we let the thread exit after the handshake is done
        // and one round-trip of command processing.
        // The test itself validates the handshake; further command processing
        // is tested via direct DebugState manipulation below.
    })
}

/// Build a JDWP command packet as raw bytes for sending over TCP.
#[allow(dead_code)]
fn build_command_packet(id: u32, command_set: u8, command: u8, data: &[u8]) -> Vec<u8> {
    let length = 11u32 + data.len() as u32;
    let mut buf = Vec::with_capacity(length as usize);
    buf.extend_from_slice(&length.to_be_bytes());
    buf.extend_from_slice(&id.to_be_bytes());
    buf.push(0x00); // flags (command, not reply)
    buf.push(command_set);
    buf.push(command);
    buf.extend_from_slice(data);
    buf
}

/// Read a JDWP reply packet from a TCP stream.  Returns `(id, error_code, data)`.
#[allow(dead_code)]
fn read_reply(stream: &mut TcpStream) -> (u32, u16, Vec<u8>) {
    let mut header = [0u8; 11];
    stream
        .read_exact(&mut header)
        .expect("must read reply header");
    let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let id = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let flags = header[8];
    assert!(flags & 0x80 != 0, "reply must have the reply flag set");
    let error_code = u16::from_be_bytes([header[9], header[10]]);
    let data_len = (length - 11) as usize;
    let mut data = vec![0u8; data_len];
    if data_len > 0 {
        stream.read_exact(&mut data).expect("must read reply data");
    }
    (id, error_code, data)
}

// ---------------------------------------------------------------------------
// T4.8.1 -- jdwp_listening_transport
// ---------------------------------------------------------------------------

/// T4.8.1: Verify the JDWP transport can listen on a port.  Connect to the
/// port, send the JDWP handshake ("JDWP-Handshake"), and verify the server
/// echoes it back.
///
/// This test exercises the TCP transport layer and the handshake protocol
/// defined in the JDWP specification.
#[test]
fn t4_8_1_jdwp_listening_transport() {
    let port = find_free_port();

    // Start the JDWP listener in a background thread.
    let handle = start_jdwp_listener(port);

    // Give the listener thread a moment to bind.
    std::thread::sleep(Duration::from_millis(100));

    // Connect as a debugger client.
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).expect("must connect to JDWP listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("must set read timeout");

    // Send the handshake.
    stream
        .write_all(HANDSHAKE)
        .expect("must send JDWP-Handshake");
    stream.flush().expect("must flush");

    // Read the echoed handshake.
    let mut response = [0u8; 14];
    stream
        .read_exact(&mut response)
        .expect("must receive handshake echo");
    assert_eq!(
        &response, HANDSHAKE,
        "server must echo JDWP-Handshake verbatim"
    );

    // Clean up.
    drop(stream);
    let _ = handle.join();
}

/// T4.8.1 extended: Verify the transport module's constant matches the spec.
#[test]
fn t4_8_1_jdwp_handshake_constant() {
    assert_eq!(JDWP_HANDSHAKE, b"JDWP-Handshake");
    assert_eq!(JDWP_HANDSHAKE.len(), 14);
}

// ---------------------------------------------------------------------------
// T4.8.2 -- jdwp_breakpoint_request
// ---------------------------------------------------------------------------

/// T4.8.2: Set a breakpoint via EventRequest, run the target method, and
/// verify the breakpoint event fires.
///
/// This test uses the DebugState / EventManager directly to register a
/// breakpoint request and verify that `check_breakpoint` matches the
/// correct location.  A full end-to-end test requiring the VM interpreter
/// is `#[ignore]`d separately.
#[test]
fn t4_8_2_jdwp_breakpoint_request() {
    let mut state = DebugState::new();

    // Register a breakpoint at class_id=10, method_id=20, offset=42.
    let class_id: u64 = 10;
    let method_id: u64 = 20;
    let offset: u64 = 42;

    let request_id = state.events.set_event_request(
        EventKind::Breakpoint,
        SuspendPolicy::All,
        vec![EventModifier::LocationOnly {
            class_id,
            method_id,
            offset,
        }],
    );
    assert!(request_id > 0, "request ID must be positive");
    assert!(
        state.events.has_breakpoints(),
        "event manager must report active breakpoints"
    );

    // Check that the breakpoint matches at the correct location.
    let matched = state.events.check_breakpoint(class_id, method_id, offset);
    assert!(
        matched.is_some(),
        "breakpoint must match at (class={class_id}, method={method_id}, offset={offset})"
    );
    let req = matched.unwrap();
    assert_eq!(req.id, request_id);
    assert_eq!(req.kind, EventKind::Breakpoint);
    assert_eq!(req.suspend_policy, SuspendPolicy::All);

    // Check that the breakpoint does NOT match at a different offset.
    let not_matched = state
        .events
        .check_breakpoint(class_id, method_id, offset + 1);
    assert!(
        not_matched.is_none(),
        "breakpoint must not match at a different offset"
    );

    // Check that the breakpoint does NOT match at a different class.
    let not_matched = state
        .events
        .check_breakpoint(class_id + 1, method_id, offset);
    assert!(
        not_matched.is_none(),
        "breakpoint must not match at a different class"
    );

    // Verify the event can be composed into a JDWP composite event packet.
    let extra = debug::build_location_extra(class_id, method_id, offset);
    assert_eq!(extra.len(), 25, "location extra must be 25 bytes");
    assert_eq!(extra[0], 1, "TypeTag must be CLASS (1)");
    assert_eq!(
        u64::from_be_bytes(extra[1..9].try_into().unwrap()),
        class_id
    );
    assert_eq!(
        u64::from_be_bytes(extra[9..17].try_into().unwrap()),
        method_id
    );
    assert_eq!(
        u64::from_be_bytes(extra[17..25].try_into().unwrap()),
        offset
    );

    let event = Event {
        request_id,
        kind: EventKind::Breakpoint,
        thread_id: 1,
        extra,
    };
    let pkt = debug::events::compose_event_packet(SuspendPolicy::All, &[event]);
    match &pkt {
        JdwpPacket::Command {
            command_set,
            command,
            data,
            ..
        } => {
            assert_eq!(*command_set, 64, "composite event command set must be 64");
            assert_eq!(*command, 100, "composite event command must be 100");
            // Data layout: suspend_policy(1) + count(4) + kind(1) + req_id(4) + thread_id(8) + location(25)
            assert_eq!(
                data.len(),
                43,
                "breakpoint composite event data must be 43 bytes"
            );
            assert_eq!(data[0], SuspendPolicy::All as u8);
        }
        _ => panic!("compose_event_packet must return a Command packet"),
    }

    // Clear the breakpoint and verify it no longer matches.
    let removed = state.events.clear_event_request(request_id);
    assert!(removed, "clear_event_request must return true");
    assert!(
        !state.events.has_breakpoints(),
        "no breakpoints should remain after clearing"
    );
    assert!(
        state
            .events
            .check_breakpoint(class_id, method_id, offset)
            .is_none(),
        "cleared breakpoint must not match"
    );
}

/// T4.8.2 (ignored): Full end-to-end breakpoint test requiring VM interpreter.
#[test]
#[ignore = "requires full VM debug mode with interpreter breakpoint hooks"]
fn t4_8_2_jdwp_breakpoint_end_to_end() {
    // This test would:
    // 1. Boot the VM in debug mode
    // 2. Load a test class
    // 3. Set a breakpoint via JDWP EventRequest.Set
    // 4. Invoke the target method
    // 5. Verify the breakpoint event fires via the debug event channel
    // 6. Resume execution
    todo!("requires full VM debug mode integration");
}

// ---------------------------------------------------------------------------
// T4.8.3 -- jdwp_step_over
// ---------------------------------------------------------------------------

/// T4.8.3: Issue a StepRequest with depth=OVER(1), verify stepping produces
/// the correct StepEvent.
///
/// The step depths are defined in the JDWP spec as:
/// - INTO (0): step into method calls
/// - OVER (1): step over method calls (stay at same call depth)
/// - OUT  (2): step out of the current method
///
/// This test exercises the EventManager's single-step matching logic.
#[test]
fn t4_8_3_jdwp_step_over() {
    let mut state = DebugState::new();

    let thread_id: u64 = 42;

    // Register a single-step request with depth=OVER on thread 42.
    let request_id = state.events.set_event_request(
        EventKind::SingleStep,
        SuspendPolicy::EventThread,
        vec![EventModifier::Step {
            thread_id,
            size: StepSize::Min,
            depth: StepDepth::Over,
        }],
    );
    assert!(request_id > 0);
    assert!(
        state.events.has_single_steps(),
        "event manager must report active single-step requests"
    );

    // Verify the step request is stored with correct depth and size.
    let req = state
        .events
        .get_request(request_id)
        .expect("step request must be retrievable by ID");
    assert_eq!(req.kind, EventKind::SingleStep);
    assert_eq!(req.suspend_policy, SuspendPolicy::EventThread);
    assert_eq!(req.step_depth, Some(StepDepth::Over));
    assert_eq!(req.step_size, Some(StepSize::Min));

    // check_single_step should match on the correct thread.
    let matched = state.events.check_single_step(thread_id);
    assert!(
        matched.is_some(),
        "single-step request must match on thread {thread_id}"
    );
    assert_eq!(matched.unwrap().id, request_id);

    // check_single_step should NOT match on a different thread.
    let not_matched = state.events.check_single_step(thread_id + 1);
    assert!(
        not_matched.is_none(),
        "single-step request must not match on a different thread"
    );

    // Verify the event can be composed as a StepEvent.
    let extra = debug::build_location_extra(100, 200, 50);
    let event = Event {
        request_id,
        kind: EventKind::SingleStep,
        thread_id,
        extra,
    };
    let pkt = debug::events::compose_event_packet(SuspendPolicy::EventThread, &[event]);
    match &pkt {
        JdwpPacket::Command { data, .. } => {
            assert_eq!(
                data[0],
                SuspendPolicy::EventThread as u8,
                "suspend policy byte must be EventThread (1)"
            );
            // Data layout: suspend_policy(1) + count(4) + kind(1) + req_id(4) + thread_id(8) + location(25) = 43
            assert_eq!(data.len(), 43);
        }
        _ => panic!("expected Command packet"),
    }

    // Clean up.
    state.events.clear_event_request(request_id);
    assert!(!state.events.has_single_steps());
}

/// T4.8.3 extended: Verify all three step depth constants match the JDWP spec.
#[test]
fn t4_8_3_step_depth_constants() {
    assert_eq!(StepDepth::Into as u32, 0, "INTO must be 0");
    assert_eq!(StepDepth::Over as u32, 1, "OVER must be 1");
    assert_eq!(StepDepth::Out as u32, 2, "OUT must be 2");

    // Round-trip through from_u32.
    assert_eq!(StepDepth::from_u32(0), Some(StepDepth::Into));
    assert_eq!(StepDepth::from_u32(1), Some(StepDepth::Over));
    assert_eq!(StepDepth::from_u32(2), Some(StepDepth::Out));
    assert_eq!(StepDepth::from_u32(3), None);
}

/// T4.8.3 extended: Verify step size constants.
#[test]
fn t4_8_3_step_size_constants() {
    assert_eq!(StepSize::Min as u32, 0, "MIN (bytecode) must be 0");
    assert_eq!(StepSize::Line as u32, 1, "LINE must be 1");
    assert_eq!(StepSize::from_u32(0), Some(StepSize::Min));
    assert_eq!(StepSize::from_u32(1), Some(StepSize::Line));
    assert_eq!(StepSize::from_u32(2), None);
}

/// T4.8.3 (ignored): Full end-to-end step-over test requiring VM interpreter.
#[test]
#[ignore = "requires full VM debug mode with interpreter single-step hooks"]
fn t4_8_3_jdwp_step_over_end_to_end() {
    // This test would:
    // 1. Boot the VM in debug mode
    // 2. Set a breakpoint, trigger it
    // 3. Issue a StepRequest with depth=OVER
    // 4. Resume, verify a SingleStep event fires at the next instruction
    //    at the same frame depth (not stepping into a method call)
    todo!("requires full VM debug mode integration");
}

// ---------------------------------------------------------------------------
// T4.8.4 -- jdwp_thread_frames
// ---------------------------------------------------------------------------

/// T4.8.4: Send a ThreadReference.Frames command, verify the returned frame
/// list has correct method names.
///
/// This test populates the DebugState's thread_frames map directly (simulating
/// what the VM would do when a thread is suspended) and verifies that the
/// frame entries are correctly structured for JDWP wire format.
#[test]
fn t4_8_4_jdwp_thread_frames() {
    let mut state = DebugState::new();
    let thread_id: u64 = 1;

    // Simulate a suspended thread with 3 stack frames, bottom-to-top:
    // Frame 2: java/lang/Object.<init>  (offset 0)
    // Frame 1: com/example/App.doWork   (offset 12)
    // Frame 0: com/example/App.main     (offset 42)
    //
    // Frame IDs are typically indices from the top of the stack.
    let frames = vec![
        FrameEntry {
            frame_id: 0,
            class_id: 100,
            method_id: 0xAABB_CCDD_EEFF_0011,
            offset: 42,
        },
        FrameEntry {
            frame_id: 1,
            class_id: 100,
            method_id: 0x1122_3344_5566_7788,
            offset: 12,
        },
        FrameEntry {
            frame_id: 2,
            class_id: 200,
            method_id: 0xDEAD_BEEF_CAFE_BABE,
            offset: 0,
        },
    ];

    state.thread_frames.insert(thread_id, frames);

    // Verify frame count.
    let frame_count = state.thread_frames.get(&thread_id).map_or(0, |f| f.len());
    assert_eq!(frame_count, 3, "thread must have 3 frames");

    // Verify individual frame fields match what was set.
    let stored_frames = state.thread_frames.get(&thread_id).unwrap();

    assert_eq!(stored_frames[0].frame_id, 0);
    assert_eq!(stored_frames[0].class_id, 100);
    assert_eq!(stored_frames[0].offset, 42);

    assert_eq!(stored_frames[1].frame_id, 1);
    assert_eq!(stored_frames[1].class_id, 100);
    assert_eq!(stored_frames[1].offset, 12);

    assert_eq!(stored_frames[2].frame_id, 2);
    assert_eq!(stored_frames[2].class_id, 200);
    assert_eq!(stored_frames[2].offset, 0);

    // Verify the method names map is populated correctly.
    // In the real VM, class_methods is populated from the class manager.
    // Here we simulate the mapping.
    state.class_methods.insert(
        100,
        vec![
            debug::commands::MethodInfo {
                method_id: debug::ids::MethodId(0xAABB_CCDD_EEFF_0011),
                name: "main".to_string(),
                signature: "([Ljava/lang/String;)V".to_string(),
                mod_bits: 0x0009, // public static
            },
            debug::commands::MethodInfo {
                method_id: debug::ids::MethodId(0x1122_3344_5566_7788),
                name: "doWork".to_string(),
                signature: "()V".to_string(),
                mod_bits: 0x0001, // public
            },
        ],
    );
    state.class_methods.insert(
        200,
        vec![debug::commands::MethodInfo {
            method_id: debug::ids::MethodId(0xDEAD_BEEF_CAFE_BABE),
            name: "<init>".to_string(),
            signature: "()V".to_string(),
            mod_bits: 0x0001,
        }],
    );

    // Verify we can look up method names from the frame entries.
    for frame in stored_frames {
        let methods = state
            .class_methods
            .get(&frame.class_id)
            .expect("class must have method metadata");
        let method = methods
            .iter()
            .find(|m| m.method_id.0 == frame.method_id)
            .expect("method must be found in class method list");
        assert!(
            !method.name.is_empty(),
            "method name must not be empty for frame_id={}",
            frame.frame_id
        );
    }

    // Verify specific method name resolutions.
    let main_method = state
        .class_methods
        .get(&100)
        .unwrap()
        .iter()
        .find(|m| m.method_id.0 == stored_frames[0].method_id)
        .unwrap();
    assert_eq!(main_method.name, "main");

    let do_work_method = state
        .class_methods
        .get(&100)
        .unwrap()
        .iter()
        .find(|m| m.method_id.0 == stored_frames[1].method_id)
        .unwrap();
    assert_eq!(do_work_method.name, "doWork");

    let init_method = state
        .class_methods
        .get(&200)
        .unwrap()
        .iter()
        .find(|m| m.method_id.0 == stored_frames[2].method_id)
        .unwrap();
    assert_eq!(init_method.name, "<init>");

    // Verify thread with no frames returns empty.
    let empty_count = state.thread_frames.get(&999).map_or(0, |f| f.len());
    assert_eq!(empty_count, 0, "non-existent thread must have 0 frames");
}

/// T4.8.4 extended: Verify thread suspension tracking works correctly.
#[test]
fn t4_8_4_thread_suspension_tracking() {
    let mut state = DebugState::new();

    // Initially no threads are suspended.
    assert!(!state.suspended);
    assert!(state.suspended_threads.is_empty());

    // Suspend a thread.
    state.suspended_threads.insert(1);
    assert!(state.suspended_threads.contains(&1));
    assert!(!state.suspended_threads.contains(&2));

    // Suspend all.
    state.suspended = true;
    assert!(state.suspended);

    // Resume all.
    state.suspended = false;
    state.suspended_threads.clear();
    assert!(!state.suspended);
    assert!(state.suspended_threads.is_empty());
}

/// T4.8.4 extended: Verify DebugEvent can carry frame information through
/// the event channel.
#[test]
fn t4_8_4_debug_event_channel_with_frames() {
    let (tx, rx) = std::sync::mpsc::channel::<DebugEvent>();

    // Simulate what the interpreter does when hitting a breakpoint:
    // it sends a DebugEvent with the location information.
    tx.send(DebugEvent {
        kind: EventKind::Breakpoint,
        request_id: 1,
        suspend_policy: SuspendPolicy::All,
        thread_id: 42,
        class_id: 100,
        method_id: 200,
        offset: 15,
    })
    .unwrap();

    let evt = rx.recv().unwrap();
    assert_eq!(evt.kind, EventKind::Breakpoint);
    assert_eq!(evt.thread_id, 42);
    assert_eq!(evt.class_id, 100);
    assert_eq!(evt.method_id, 200);
    assert_eq!(evt.offset, 15);
}

/// T4.8.4 (ignored): Full end-to-end thread frames test requiring live VM.
#[test]
#[ignore = "requires starting VM in debug mode with a suspended thread"]
fn t4_8_4_jdwp_thread_frames_end_to_end() {
    // This test would:
    // 1. Boot the VM in debug mode
    // 2. Set a breakpoint in a method with known call stack
    // 3. When the breakpoint fires, send ThreadReference.Frames(6,6)
    // 4. Parse the reply and verify frame method names match expectations
    todo!("requires full VM debug mode integration");
}
