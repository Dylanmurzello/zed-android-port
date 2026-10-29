//! Properties of the translation core and the filter, over generated
//! paths and syscall data. The unit tests pin down examples; these pin
//! down the invariants the handler relies on.

use proptest::prelude::*;
use zd_engine_core::bindings::Bindings;
use zd_engine_core::bpf::{
    AUDIT_ARCH_AARCH64, FilterSpec, Program, SECCOMP_RET_ALLOW, SECCOMP_RET_TRAP, SeccompData, build, evaluate,
};
use zd_engine_core::path::{MAX_PATH, clamp_to_root, components, depth, normalize, strip_prefix};
use zd_engine_core::sysno::{PROBE_MAGIC, TAG_PATH, TAG_PROBE, TRAP_TABLE, nr};
use zd_engine_core::translate::{Translated, Translator};
use zd_engine_core::{LOADER_IMAGE_BASE, LOADER_WINDOW_SIZE};

const ROOTFS: &[u8] = b"/data/user/0/com.zdroid/files/rootfs";
const BINDS: &[u8] = b"/data/user/0/com.zdroid/files;/data/data/com.zdroid/files:/data/user/0/com.zdroid/files;/tmp:/data/user/0/com.zdroid/files/tmp;/sdcard;/storage";

fn translator() -> Translator<'static> {
    let mut bindings = Bindings::with_defaults();
    bindings.parse_spec(BINDS).unwrap();
    Translator::new(ROOTFS, bindings)
}

/// A guest absolute path from a small alphabet, with dots and empty
/// components mixed in; `proc` and `tmp` reach the special cases.
fn guest_path() -> impl Strategy<Value = Vec<u8>> {
    let component = prop_oneof![
        Just(b"a".to_vec()),
        Just(b"bb".to_vec()),
        Just(b"..".to_vec()),
        Just(b".".to_vec()),
        Just(b"".to_vec()),
        Just(b"tmp".to_vec()),
        Just(b"proc".to_vec()),
        Just(b"self".to_vec()),
        Just(b"cwd".to_vec()),
        Just(b"root".to_vec()),
        Just(b"exe".to_vec()),
        Just(b"sdcard".to_vec()),
    ];
    prop::collection::vec(component, 0..8).prop_map(|parts| {
        let mut path = Vec::new();
        for part in parts {
            path.push(b'/');
            path.extend_from_slice(&part);
        }
        if path.is_empty() {
            path.push(b'/');
        }
        path
    })
}

fn running_depth_never_negative(path: &[u8]) -> bool {
    let mut level: i64 = 0;
    for part in components(path) {
        if part == b".." {
            level -= 1;
        } else {
            level += 1;
        }
        if level < 0 {
            return false;
        }
    }
    true
}

proptest! {
    #[test]
    fn clamp_drops_only_climbs_above_the_root(path in guest_path()) {
        let mut out = [0u8; MAX_PATH];
        let len = clamp_to_root(&path, &mut out).unwrap();
        let clamped = &out[..len];
        prop_assert_eq!(clamped[0], b'/');
        prop_assert!(running_depth_never_negative(clamped));
        prop_assert_eq!(depth(clamped), depth(&path));
        // Every kept component appears in the input in order.
        let input: Vec<&[u8]> = components(&path).collect();
        let mut cursor = 0;
        for part in components(clamped) {
            let found = input[cursor..].iter().position(|candidate| *candidate == part);
            prop_assert!(found.is_some());
            cursor += found.unwrap() + 1;
        }
    }

    #[test]
    fn normalize_removes_every_dotdot(path in guest_path()) {
        let mut out = [0u8; MAX_PATH];
        let len = normalize(&path, &mut out).unwrap();
        let normalized = &out[..len];
        prop_assert_eq!(normalized[0], b'/');
        prop_assert!(components(normalized).all(|part| part != b".."));
        prop_assert_eq!(depth(normalized), depth(&path));
    }

    #[test]
    fn translated_paths_start_in_the_rootfs_or_a_binding(path in guest_path()) {
        let translator = translator();
        let mut buffer = [0u8; MAX_PATH];
        match translator.guest_to_host(&path, &mut buffer).unwrap() {
            Translated::Host(len) => {
                let host = &buffer[..len];
                let inside = strip_prefix(ROOTFS, host).is_some()
                    || translator.bindings.iter().any(|binding| strip_prefix(binding.host, host).is_some());
                prop_assert!(inside, "{:?} -> {:?}", String::from_utf8_lossy(&path), String::from_utf8_lossy(host));
                prop_assert_eq!(buffer[len], 0);
            }
            Translated::Relative => prop_assert!(false, "absolute input came back relative"),
            Translated::ProcExe(_)
            | Translated::ProcAuxv(_)
            | Translated::ProcLink { .. }
            | Translated::ProcSynth(_) => {}
        }
    }

    #[test]
    fn host_to_guest_undoes_guest_to_host(path in guest_path()) {
        let translator = translator();
        let mut host = [0u8; MAX_PATH];
        let Translated::Host(host_len) = translator.guest_to_host(&path, &mut host).unwrap() else {
            return Ok(());
        };
        let mut back = [0u8; MAX_PATH];
        let back_len = translator.host_to_guest(&host[..host_len], &mut back).unwrap();
        let mut expected = [0u8; MAX_PATH];
        let mut expected_len = clamp_to_root(&path, &mut expected).unwrap();
        // `/proc/<who>/root/<rest>` is re-anchored before the clamp, so the
        // expectation is the clamped rest.
        let mut guest: &[u8] = &path;
        while let Some(rest) = root_link_rest(guest) {
            guest = rest;
            expected_len = clamp_to_root(guest, &mut expected).unwrap();
        }
        prop_assert_eq!(
            String::from_utf8_lossy(&back[..back_len]),
            String::from_utf8_lossy(&expected[..expected_len])
        );
    }

    #[test]
    fn filter_classes(
        trapped_index in 0..TRAP_TABLE.len(),
        untrapped in prop_oneof![Just(nr::READ), Just(nr::WRITE), Just(nr::MMAP), Just(nr::CLONE), Just(nr::MUNMAP)],
        offset in 0u64..LOADER_WINDOW_SIZE,
        outside in prop_oneof![Just(0x7f00_0000_0000u64), Just(0x5500_0000_0000u64), Just(0x53ff_ffff_fff0u64), Just(0x1000u64)],
        arg0 in any::<u64>(),
    ) {
        let program = program();
        let trap = SECCOMP_RET_TRAP | u32::from(TAG_PATH);
        let trapped = TRAP_TABLE[trapped_index];
        prop_assert_eq!(evaluate(&program, &data(trapped, outside, arg0)), trap);
        prop_assert_eq!(evaluate(&program, &data(trapped, LOADER_IMAGE_BASE + offset, arg0)), SECCOMP_RET_ALLOW);
        prop_assert_eq!(evaluate(&program, &data(untrapped, outside, arg0)), SECCOMP_RET_ALLOW);
        prop_assert_eq!(evaluate(&program, &data(untrapped, LOADER_IMAGE_BASE + offset, arg0)), SECCOMP_RET_ALLOW);
        let probe = SECCOMP_RET_TRAP | u32::from(TAG_PROBE);
        prop_assert_eq!(evaluate(&program, &data(nr::GETPID, outside, PROBE_MAGIC)), probe);
        prop_assert_eq!(evaluate(&program, &data(nr::GETPID, LOADER_IMAGE_BASE + offset, PROBE_MAGIC)), probe);
        if arg0 != PROBE_MAGIC {
            prop_assert_eq!(evaluate(&program, &data(nr::GETPID, outside, arg0)), SECCOMP_RET_ALLOW);
        }
    }
}

/// What follows a `/proc/<who>/root` prefix, when the path has one and
/// something follows (a bare link stays the kernel's).
fn root_link_rest(path: &[u8]) -> Option<&[u8]> {
    let rest = strip_prefix(b"/proc", path)?;
    let mut parts = components(rest);
    let who = parts.next()?;
    let is_who = who == b"self" || who == b"thread-self" || who.iter().all(u8::is_ascii_digit);
    let root = parts.next()?;
    if !is_who || root != b"root" {
        return None;
    }
    let end = root.as_ptr() as usize + root.len() - path.as_ptr() as usize;
    let after = &path[end..];
    if after.is_empty() { None } else { Some(after) }
}

fn program() -> Program {
    build(&FilterSpec {
        trapped: TRAP_TABLE,
        arg_traps: &[],
        window_start: LOADER_IMAGE_BASE,
        window_end: LOADER_IMAGE_BASE + LOADER_WINDOW_SIZE,
        probe_nr: nr::GETPID,
        probe_magic: PROBE_MAGIC,
        path_tag: TAG_PATH,
        probe_tag: TAG_PROBE,
    })
    .unwrap()
}

fn data(nr: u32, ip: u64, arg0: u64) -> SeccompData {
    SeccompData {
        nr,
        arch: AUDIT_ARCH_AARCH64,
        instruction_pointer: ip,
        args: [arg0, 0, 0, 0, 0, 0],
    }
}
