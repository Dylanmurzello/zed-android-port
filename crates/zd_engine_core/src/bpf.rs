//! Builder for the engine's seccomp-bpf program, plus a classic-BPF
//! interpreter under `std` so the program can be checked on the host.
//!
//! Program shape:
//! 1. `arch != AArch64` -> ALLOW (never expected; stay out of the way).
//! 2. `getpid(PROBE_MAGIC)` -> TRAP tagged `probe_tag`.
//! 3. instruction pointer inside the loader window -> ALLOW (this is how
//!    the handler's own re-issued syscalls get through).
//! 4. argument-conditional traps (`ioctl` with a termios2 request) ->
//!    TRAP tagged `path_tag`; other arguments fall through.
//! 5. trapped syscall numbers -> TRAP tagged `path_tag`.
//! 6. everything else -> ALLOW.

/// `struct sock_filter` from `<linux/filter.h>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

pub const MAX_INSNS: usize = 256;

pub const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
pub const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
pub const SECCOMP_RET_DATA: u32 = 0x0000_ffff;
pub const AUDIT_ARCH_AARCH64: u32 = 0xC000_00B7;

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x15;
const BPF_JMP_JGE_K: u16 = 0x35;
const BPF_RET_K: u16 = 0x06;

/// Byte offsets into `struct seccomp_data`.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
const OFF_IP_LO: u32 = 8;
const OFF_IP_HI: u32 = 12;
const OFF_ARG0_LO: u32 = 16;
const OFF_ARG0_HI: u32 = 20;

/// Trap `nr` only when argument `arg` equals one of `values`, or, with
/// `unless`, only when it equals none of them.
#[derive(Debug, Clone, Copy)]
pub struct ArgTrap<'a> {
    pub nr: u32,
    pub arg: u8,
    pub values: &'a [u64],
    pub unless: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BpfError {
    TooManyTrapped,
    /// The window must stay inside one 4 GiB region so the instruction
    /// pointer test is two 32-bit compares.
    WindowSpansHighWord,
}

pub struct FilterSpec<'a> {
    pub trapped: &'a [u32],
    pub arg_traps: &'a [ArgTrap<'a>],
    pub window_start: u64,
    pub window_end: u64,
    pub probe_nr: u32,
    pub probe_magic: u64,
    pub path_tag: u16,
    pub probe_tag: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct Program {
    insns: [SockFilter; MAX_INSNS],
    len: usize,
}

impl Program {
    pub fn insns(&self) -> &[SockFilter] {
        &self.insns[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

const fn ld(k: u32) -> SockFilter {
    SockFilter {
        code: BPF_LD_W_ABS,
        jt: 0,
        jf: 0,
        k,
    }
}

const fn jeq(k: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter {
        code: BPF_JMP_JEQ_K,
        jt,
        jf,
        k,
    }
}

const fn jge(k: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter {
        code: BPF_JMP_JGE_K,
        jt,
        jf,
        k,
    }
}

const fn ret(k: u32) -> SockFilter {
    SockFilter {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k,
    }
}

fn arg_offset(arg: u8) -> u32 {
    OFF_ARG0_LO + 8 * u32::from(arg)
}

pub fn build(spec: &FilterSpec) -> Result<Program, BpfError> {
    // Fixed prologue (indices 0..=15), one block per argument trap, one
    // jeq per trapped number, then ALLOW for the fall-through and the
    // shared TRAP that the jumps reach. Jump offsets are relative to the
    // next instruction, so every forward jump below is `target - index - 1`.
    const PROLOGUE: usize = 16;
    let count = spec.trapped.len();
    let arg_block_len: usize = spec
        .arg_traps
        .iter()
        .map(|trap| 2 + 4 * trap.values.len())
        .sum();
    if PROLOGUE + arg_block_len + count + 2 > MAX_INSNS || count > 200 {
        return Err(BpfError::TooManyTrapped);
    }
    let window_hi = (spec.window_start >> 32) as u32;
    if (spec.window_end - 1) >> 32 != u64::from(window_hi) {
        return Err(BpfError::WindowSpansHighWord);
    }
    let window_lo_start = spec.window_start as u32;
    let window_lo_end = spec.window_end as u32;
    let table_index = PROLOGUE + arg_block_len;
    let allow_index = table_index + count;
    let trap_index = allow_index + 1;
    let total = trap_index + 1;
    if trap_index - PROLOGUE > usize::from(u8::MAX) {
        return Err(BpfError::TooManyTrapped);
    }

    let mut insns = [ret(SECCOMP_RET_ALLOW); MAX_INSNS];
    let distance = |from: usize, to: usize| -> u8 { (to - from - 1) as u8 };

    insns[0] = ld(OFF_ARCH);
    insns[1] = jeq(AUDIT_ARCH_AARCH64, 0, distance(1, allow_index));
    insns[2] = ld(OFF_NR);
    insns[3] = jeq(spec.probe_nr, 0, distance(3, 9));
    insns[4] = ld(OFF_ARG0_LO);
    insns[5] = jeq(spec.probe_magic as u32, 0, distance(5, 9));
    insns[6] = ld(OFF_ARG0_HI);
    insns[7] = jeq((spec.probe_magic >> 32) as u32, 0, distance(7, 9));
    insns[8] = ret(SECCOMP_RET_TRAP | u32::from(spec.probe_tag));
    insns[9] = ld(OFF_IP_HI);
    insns[10] = jeq(window_hi, 0, distance(10, 15));
    insns[11] = ld(OFF_IP_LO);
    insns[12] = jge(window_lo_start, 0, distance(12, 15));
    insns[13] = jge(window_lo_end, distance(13, 15), 0);
    insns[14] = ret(SECCOMP_RET_ALLOW);
    insns[15] = ld(OFF_NR);

    // Each block: `jeq nr` else skip the block (the accumulator still
    // holds nr); per value `ld lo; jne -> next value; ld hi; jeq -> TRAP`
    // (or -> ALLOW for an `unless` block); a plain block ends by reloading
    // nr for whatever follows, an `unless` block by trapping.
    let mut index = PROLOGUE;
    for trap in spec.arg_traps {
        let block_end = index + 2 + 4 * trap.values.len();
        insns[index] = jeq(trap.nr, 0, distance(index, block_end));
        index += 1;
        let on_match = if trap.unless { allow_index } else { trap_index };
        for &value in trap.values {
            let next_value = index + 4;
            insns[index] = ld(arg_offset(trap.arg));
            insns[index + 1] = jeq(value as u32, 0, distance(index + 1, next_value));
            insns[index + 2] = ld(arg_offset(trap.arg) + 4);
            insns[index + 3] = jeq((value >> 32) as u32, distance(index + 3, on_match), 0);
            index = next_value;
        }
        insns[index] = if trap.unless {
            ret(SECCOMP_RET_TRAP | u32::from(spec.path_tag))
        } else {
            ld(OFF_NR)
        };
        index += 1;
        debug_assert_eq!(index, block_end);
    }

    for (offset, &number) in spec.trapped.iter().enumerate() {
        let index = table_index + offset;
        insns[index] = jeq(number, distance(index, trap_index), 0);
    }
    insns[trap_index] = ret(SECCOMP_RET_TRAP | u32::from(spec.path_tag));
    insns[allow_index] = ret(SECCOMP_RET_ALLOW);

    Ok(Program { insns, len: total })
}

/// `struct seccomp_data` as the kernel hands it to the filter.
#[cfg(feature = "std")]
#[derive(Debug, Clone, Copy)]
pub struct SeccompData {
    pub nr: u32,
    pub arch: u32,
    pub instruction_pointer: u64,
    pub args: [u64; 6],
}

/// Evaluate `program` the way the kernel's classic-BPF interpreter
/// would, for host tests.
#[cfg(feature = "std")]
pub fn evaluate(program: &Program, data: &SeccompData) -> u32 {
    let load = |offset: u32| -> u32 {
        match offset {
            OFF_NR => data.nr,
            OFF_ARCH => data.arch,
            OFF_IP_LO => data.instruction_pointer as u32,
            OFF_IP_HI => (data.instruction_pointer >> 32) as u32,
            other => {
                let rel = other - OFF_ARG0_LO;
                let arg = data.args[(rel / 8) as usize];
                if rel.is_multiple_of(8) { arg as u32 } else { (arg >> 32) as u32 }
            }
        }
    };
    let insns = program.insns();
    let mut pc = 0usize;
    let mut acc = 0u32;
    loop {
        let insn = insns[pc];
        match insn.code {
            BPF_LD_W_ABS => acc = load(insn.k),
            BPF_JMP_JEQ_K => pc += usize::from(if acc == insn.k { insn.jt } else { insn.jf }),
            BPF_JMP_JGE_K => pc += usize::from(if acc >= insn.k { insn.jt } else { insn.jf }),
            BPF_RET_K => return insn.k,
            other => panic!("unexpected bpf opcode {other:#x} at {pc}"),
        }
        pc += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysno::{PROBE_MAGIC, TAG_PATH, TAG_PROBE, TRAP_TABLE_CHUNK1, nr};
    use crate::{LOADER_IMAGE_BASE, LOADER_WINDOW_SIZE};

    const IOCTL_TRAPS: &[ArgTrap<'static>] = &[ArgTrap {
        nr: nr::IOCTL,
        arg: 1,
        values: crate::sysno::ioctl::TERMIOS2_REQUESTS,
        unless: false,
    }];

    #[test]
    fn address_outputs_trap_only_with_a_buffer() {
        use crate::sysno::address_arg;
        const TRAPS: &[ArgTrap<'static>] = &[
            ArgTrap {
                nr: nr::RECVFROM,
                arg: 4,
                values: address_arg::NULL,
                unless: true,
            },
            ArgTrap {
                nr: nr::ACCEPT4,
                arg: 1,
                values: address_arg::NULL,
                unless: true,
            },
        ];
        let program = build(&FilterSpec {
            trapped: TRAP_TABLE_CHUNK1,
            arg_traps: TRAPS,
            window_start: LOADER_IMAGE_BASE,
            window_end: LOADER_IMAGE_BASE + LOADER_WINDOW_SIZE,
            probe_nr: nr::GETPID,
            probe_magic: PROBE_MAGIC,
            path_tag: TAG_PATH,
            probe_tag: TAG_PROBE,
        })
        .unwrap();
        let trap = SECCOMP_RET_TRAP | u32::from(TAG_PATH);
        let mut call = data(nr::RECVFROM, GUEST_IP, 3);
        assert_eq!(evaluate(&program, &call), SECCOMP_RET_ALLOW);
        call.args[4] = 0x7000;
        assert_eq!(evaluate(&program, &call), trap);
        let mut call = data(nr::ACCEPT4, GUEST_IP, 3);
        assert_eq!(evaluate(&program, &call), SECCOMP_RET_ALLOW);
        call.args[1] = 0x7000;
        assert_eq!(evaluate(&program, &call), trap);
    }

    #[test]
    fn clone3_traps_unless_the_struct_is_ver0_sized() {
        use crate::sysno::clone;
        const TRAPS: &[ArgTrap<'static>] = &[
            IOCTL_TRAPS[0],
            ArgTrap {
                nr: nr::CLONE3,
                arg: 1,
                values: clone::PASSED_SIZES,
                unless: true,
            },
        ];
        let program = build(&FilterSpec {
            trapped: TRAP_TABLE_CHUNK1,
            arg_traps: TRAPS,
            window_start: LOADER_IMAGE_BASE,
            window_end: LOADER_IMAGE_BASE + LOADER_WINDOW_SIZE,
            probe_nr: nr::GETPID,
            probe_magic: PROBE_MAGIC,
            path_tag: TAG_PATH,
            probe_tag: TAG_PROBE,
        })
        .unwrap();
        let trap = SECCOMP_RET_TRAP | u32::from(TAG_PATH);
        let mut call = data(nr::CLONE3, GUEST_IP, 0x1000);
        for size in [88, 80, 65, 4096, clone::CLONE_ARGS_SIZE_VER0 | (1 << 32)] {
            call.args[1] = size;
            assert_eq!(evaluate(&program, &call), trap, "{size:#x}");
        }
        call.args[1] = clone::CLONE_ARGS_SIZE_VER0;
        assert_eq!(evaluate(&program, &call), SECCOMP_RET_ALLOW);
        call.instruction_pointer = LOADER_IMAGE_BASE + 0x100;
        call.args[1] = 88;
        assert_eq!(evaluate(&program, &call), SECCOMP_RET_ALLOW);
        let mut ioctl = data(nr::IOCTL, GUEST_IP, 0);
        ioctl.args[1] = crate::sysno::ioctl::TCGETS2;
        assert_eq!(evaluate(&program, &ioctl), trap);
        assert_eq!(evaluate(&program, &data(nr::OPENAT, GUEST_IP, 0)), trap);
        assert_eq!(evaluate(&program, &data(nr::READ, GUEST_IP, 0)), SECCOMP_RET_ALLOW);
    }

    fn program() -> Program {
        build(&FilterSpec {
            trapped: TRAP_TABLE_CHUNK1,
            arg_traps: IOCTL_TRAPS,
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

    #[test]
    fn ioctl_traps_only_termios2_requests() {
        use crate::sysno::ioctl;
        let program = program();
        let mut call = data(nr::IOCTL, GUEST_IP, 0);
        for request in [ioctl::TCGETS2, ioctl::TCSETS2, ioctl::TCSETSW2, ioctl::TCSETSF2] {
            call.args[1] = request;
            assert_eq!(evaluate(&program, &call), SECCOMP_RET_TRAP | u32::from(TAG_PATH), "{request:#x}");
        }
        for request in [ioctl::TCGETS, ioctl::TCSETS, 0x5413, 0x8912, ioctl::TCGETS2 | (1 << 32)] {
            call.args[1] = request;
            assert_eq!(evaluate(&program, &call), SECCOMP_RET_ALLOW, "{request:#x}");
        }
        call.args[1] = ioctl::TCGETS2;
        call.instruction_pointer = LOADER_IP;
        assert_eq!(evaluate(&program, &call), SECCOMP_RET_ALLOW);
        // A trapped number after the ioctl block still traps: the block
        // reloads the syscall number before the table.
        assert_eq!(evaluate(&program, &data(nr::OPENAT, GUEST_IP, ioctl::TCGETS2)), SECCOMP_RET_TRAP | u32::from(TAG_PATH));
    }

    const GUEST_IP: u64 = 0x7f12_3456_7890;
    const LOADER_IP: u64 = LOADER_IMAGE_BASE + 0x41fc;

    #[test]
    fn traps_path_syscalls_from_guest_code() {
        let program = program();
        for &number in TRAP_TABLE_CHUNK1 {
            assert_eq!(
                evaluate(&program, &data(number, GUEST_IP, 0)),
                SECCOMP_RET_TRAP | u32::from(TAG_PATH)
            );
        }
    }

    #[test]
    fn allows_loader_syscalls_inside_window() {
        let program = program();
        for &number in TRAP_TABLE_CHUNK1 {
            assert_eq!(evaluate(&program, &data(number, LOADER_IP, 0)), SECCOMP_RET_ALLOW);
        }
        let last_in_window = LOADER_IMAGE_BASE + LOADER_WINDOW_SIZE - 4;
        assert_eq!(evaluate(&program, &data(nr::OPENAT, last_in_window, 0)), SECCOMP_RET_ALLOW);
    }

    #[test]
    fn window_edges_trap() {
        let program = program();
        let just_past = LOADER_IMAGE_BASE + LOADER_WINDOW_SIZE;
        assert_eq!(
            evaluate(&program, &data(nr::OPENAT, just_past, 0)),
            SECCOMP_RET_TRAP | u32::from(TAG_PATH)
        );
        let wrong_high_word = 0x55_0000_0000 + 0x41fc;
        assert_eq!(
            evaluate(&program, &data(nr::OPENAT, wrong_high_word, 0)),
            SECCOMP_RET_TRAP | u32::from(TAG_PATH)
        );
        let just_before = LOADER_IMAGE_BASE - 4;
        assert_eq!(
            evaluate(&program, &data(nr::OPENAT, just_before, 0)),
            SECCOMP_RET_TRAP | u32::from(TAG_PATH)
        );
    }

    #[test]
    fn untrapped_syscalls_pass_anywhere() {
        let program = program();
        for number in [nr::READ, nr::WRITE, nr::MMAP, nr::CLONE, nr::EXECVE, nr::GETCWD] {
            assert_eq!(evaluate(&program, &data(number, GUEST_IP, 0)), SECCOMP_RET_ALLOW);
        }
    }

    #[test]
    fn probe_needs_the_magic() {
        let program = program();
        assert_eq!(
            evaluate(&program, &data(nr::GETPID, GUEST_IP, PROBE_MAGIC)),
            SECCOMP_RET_TRAP | u32::from(TAG_PROBE)
        );
        assert_eq!(evaluate(&program, &data(nr::GETPID, GUEST_IP, 0)), SECCOMP_RET_ALLOW);
        assert_eq!(
            evaluate(&program, &data(nr::GETPID, GUEST_IP, PROBE_MAGIC ^ (1 << 40))),
            SECCOMP_RET_ALLOW
        );
    }

    #[test]
    fn other_architectures_pass() {
        let program = program();
        let mut foreign = data(nr::OPENAT, GUEST_IP, 0);
        foreign.arch = 0x4000_0028;
        assert_eq!(evaluate(&program, &foreign), SECCOMP_RET_ALLOW);
    }

    #[test]
    fn jumps_stay_inside_the_program() {
        let program = program();
        let len = program.len();
        for (index, insn) in program.insns().iter().enumerate() {
            if insn.code == BPF_JMP_JEQ_K || insn.code == BPF_JMP_JGE_K {
                assert!(index + 1 + usize::from(insn.jt) < len, "jt out of range at {index}");
                assert!(index + 1 + usize::from(insn.jf) < len, "jf out of range at {index}");
            }
        }
        assert_eq!(len, 16 + (2 + 4 * 4) + TRAP_TABLE_CHUNK1.len() + 2);
    }

    #[test]
    fn rejects_windows_crossing_a_high_word() {
        let error = build(&FilterSpec {
            trapped: TRAP_TABLE_CHUNK1,
            arg_traps: &[],
            window_start: 0x54_FFFF_0000,
            window_end: 0x55_0000_1000,
            probe_nr: nr::GETPID,
            probe_magic: PROBE_MAGIC,
            path_tag: TAG_PATH,
            probe_tag: TAG_PROBE,
        })
        .unwrap_err();
        assert_eq!(error, BpfError::WindowSpansHighWord);
    }
}
