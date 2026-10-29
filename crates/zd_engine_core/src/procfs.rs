//! Parsers for the `/proc` files the handler consults, and the catalog
//! of the `/proc` files the Android app sandbox refuses that the engine
//! serves itself.

use crate::path::strip_prefix;

/// The `TracerPid` field of a `/proc/<pid>/status` (or `thread-self`)
/// file: 0 when nothing traces the task, else the tracer's pid. `None`
/// when the field is missing from the bytes given.
pub fn tracer_pid(status: &[u8]) -> Option<u32> {
    let key = b"TracerPid:";
    let line = status
        .split(|&byte| byte == b'\n')
        .find(|line| line.starts_with(key))?;
    let mut value = 0u32;
    let mut digits = 0;
    for &byte in &line[key.len()..] {
        match byte {
            b' ' | b'\t' if digits == 0 => continue,
            b'0'..=b'9' => {
                value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
                digits += 1;
            }
            _ => break,
        }
    }
    (digits > 0).then_some(value)
}

/// A file under `/proc` the Android app sandbox refuses to the process
/// (`EACCES` on open, invisible to stat) that the engine serves with
/// content it can derive from what the sandbox does allow. The kernel's
/// own file is tried first, so a device that allows it keeps the real one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynthFile {
    /// `/proc/stat`: one `cpu<n>` line per online cpu, zero jiffies.
    Stat,
    Loadavg,
    Uptime,
    Version,
    Vmstat,
    Diskstats,
    Swaps,
    Filesystems,
    Cmdline,
    Modules,
    Partitions,
    NetDev,
    NetTcp,
    NetTcp6,
    NetUdp,
    NetUdp6,
    NetUnix,
    NetArp,
    NetRoute,
    NetIfInet6,
    SysPidMax,
    SysOsRelease,
    SysOsType,
    SysHostname,
    SysVersion,
    SysNgroupsMax,
    SysThreadsMax,
    SysOvercommitMemory,
    SysMaxMapCount,
    SysMmapMinAddr,
    SysFileMax,
    SysFileNr,
    SysPipeMaxSize,
    SysInotifyMaxUserWatches,
    SysSomaxconn,
    SysIpLocalPortRange,
}

/// The served file named by a clamped guest path, if any.
pub fn classify(guest: &[u8]) -> Option<SynthFile> {
    let rest = strip_prefix(b"/proc", guest)?;
    Some(match rest {
        b"/stat" => SynthFile::Stat,
        b"/loadavg" => SynthFile::Loadavg,
        b"/uptime" => SynthFile::Uptime,
        b"/version" => SynthFile::Version,
        b"/vmstat" => SynthFile::Vmstat,
        b"/diskstats" => SynthFile::Diskstats,
        b"/swaps" => SynthFile::Swaps,
        b"/filesystems" => SynthFile::Filesystems,
        b"/cmdline" => SynthFile::Cmdline,
        b"/modules" => SynthFile::Modules,
        b"/partitions" => SynthFile::Partitions,
        b"/net/dev" => SynthFile::NetDev,
        b"/net/tcp" => SynthFile::NetTcp,
        b"/net/tcp6" => SynthFile::NetTcp6,
        b"/net/udp" => SynthFile::NetUdp,
        b"/net/udp6" => SynthFile::NetUdp6,
        b"/net/unix" => SynthFile::NetUnix,
        b"/net/arp" => SynthFile::NetArp,
        b"/net/route" => SynthFile::NetRoute,
        b"/net/if_inet6" => SynthFile::NetIfInet6,
        b"/sys/kernel/pid_max" => SynthFile::SysPidMax,
        b"/sys/kernel/osrelease" => SynthFile::SysOsRelease,
        b"/sys/kernel/ostype" => SynthFile::SysOsType,
        b"/sys/kernel/hostname" => SynthFile::SysHostname,
        b"/sys/kernel/version" => SynthFile::SysVersion,
        b"/sys/kernel/ngroups_max" => SynthFile::SysNgroupsMax,
        b"/sys/kernel/threads-max" => SynthFile::SysThreadsMax,
        b"/sys/vm/overcommit_memory" => SynthFile::SysOvercommitMemory,
        b"/sys/vm/max_map_count" => SynthFile::SysMaxMapCount,
        b"/sys/vm/mmap_min_addr" => SynthFile::SysMmapMinAddr,
        b"/sys/fs/file-max" => SynthFile::SysFileMax,
        b"/sys/fs/file-nr" => SynthFile::SysFileNr,
        b"/sys/fs/pipe-max-size" => SynthFile::SysPipeMaxSize,
        b"/sys/fs/inotify/max_user_watches" => SynthFile::SysInotifyMaxUserWatches,
        b"/sys/net/core/somaxconn" => SynthFile::SysSomaxconn,
        b"/sys/net/ipv4/ip_local_port_range" => SynthFile::SysIpLocalPortRange,
        _ => return None,
    })
}

/// The text of a served file that does not depend on the machine; `None`
/// for the ones the loader computes.
pub fn constant_content(file: SynthFile) -> Option<&'static [u8]> {
    Some(match file {
        SynthFile::Diskstats | SynthFile::Modules | SynthFile::NetIfInet6 => b"",
        SynthFile::Cmdline => b"\n",
        SynthFile::Swaps => b"Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n",
        SynthFile::Filesystems => b"nodev\tsysfs\nnodev\ttmpfs\nnodev\tbdev\nnodev\tproc\nnodev\tcgroup\n\
            nodev\tcgroup2\nnodev\tcpuset\nnodev\tdevtmpfs\nnodev\tbinfmt_misc\nnodev\tdebugfs\n\
            nodev\ttracefs\nnodev\tsockfs\nnodev\tbpf\nnodev\tpipefs\nnodev\tramfs\nnodev\thugetlbfs\n\
            nodev\tdevpts\n\text4\n\tvfat\n\tf2fs\nnodev\tfuse\n\tfuseblk\nnodev\tfusectl\n\
            nodev\toverlay\n\tsquashfs\nnodev\tmqueue\nnodev\tconfigfs\n",
        SynthFile::Partitions => b"major minor  #blocks  name\n\n",
        SynthFile::NetDev => b"Inter-|   Receive                                                |  Transmit\n\
             face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
                lo:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0\n",
        SynthFile::NetTcp => b"  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
        SynthFile::NetTcp6 => b"  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
        SynthFile::NetUdp => b"   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n",
        SynthFile::NetUdp6 => b"   sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n",
        SynthFile::NetUnix => b"Num       RefCount Protocol Flags    Type St Inode Path\n",
        SynthFile::NetArp => b"IP address       HW type     Flags       HW address            Mask     Device\n",
        SynthFile::NetRoute => b"Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n",
        SynthFile::SysPidMax => b"4194304\n",
        SynthFile::SysOsType => b"Linux\n",
        SynthFile::SysNgroupsMax => b"65536\n",
        SynthFile::SysThreadsMax => b"131072\n",
        SynthFile::SysOvercommitMemory => b"0\n",
        SynthFile::SysMaxMapCount => b"65530\n",
        SynthFile::SysMmapMinAddr => b"32768\n",
        SynthFile::SysFileMax => b"1048576\n",
        SynthFile::SysFileNr => b"0\t0\t1048576\n",
        SynthFile::SysPipeMaxSize => b"1048576\n",
        SynthFile::SysInotifyMaxUserWatches => b"524288\n",
        SynthFile::SysSomaxconn => b"4096\n",
        SynthFile::SysIpLocalPortRange => b"32768\t60999\n",
        SynthFile::Stat
        | SynthFile::Loadavg
        | SynthFile::Uptime
        | SynthFile::Version
        | SynthFile::Vmstat
        | SynthFile::SysOsRelease
        | SynthFile::SysHostname
        | SynthFile::SysVersion => return None,
    })
}

/// The cpu ids named by a `/sys/devices/system/cpu/online` list such as
/// `0-3,5,7-8`; malformed input ends the iteration.
pub fn cpu_list(text: &[u8]) -> CpuList<'_> {
    CpuList { text, range: None }
}

pub struct CpuList<'a> {
    text: &'a [u8],
    range: Option<(u32, u32)>,
}

impl Iterator for CpuList<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        loop {
            if let Some((next, end)) = self.range {
                self.range = (next < end).then(|| (next + 1, end));
                return Some(next);
            }
            while let Some((&byte, rest)) = self.text.split_first() {
                if byte == b',' || byte == b' ' || byte == b'\n' {
                    self.text = rest;
                } else {
                    break;
                }
            }
            let (start, rest) = number(self.text)?;
            let (end, rest) = match rest.split_first() {
                Some((b'-', rest)) => number(rest)?,
                _ => (start, rest),
            };
            if end < start {
                return None;
            }
            self.text = rest;
            self.range = Some((start, end));
        }
    }
}

fn number(text: &[u8]) -> Option<(u32, &[u8])> {
    let digits = text.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let mut value = 0u32;
    for &byte in &text[..digits] {
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    Some((value, &text[digits..]))
}

#[cfg(test)]
mod tests {
    use super::{SynthFile, classify, constant_content, cpu_list, tracer_pid};

    #[test]
    fn classifies_the_served_files_only() {
        assert_eq!(classify(b"/proc/stat"), Some(SynthFile::Stat));
        assert_eq!(
            classify(b"/proc/sys/kernel/pid_max"),
            Some(SynthFile::SysPidMax)
        );
        assert_eq!(classify(b"/proc/net/dev"), Some(SynthFile::NetDev));
        assert_eq!(classify(b"/proc/self/stat"), None);
        assert_eq!(classify(b"/proc/stat/"), None);
        assert_eq!(classify(b"/proc/statx"), None);
        assert_eq!(classify(b"/procs/stat"), None);
        assert_eq!(classify(b"/proc"), None);
        assert_eq!(classify(b"/usr/lib"), None);
    }

    #[test]
    fn constants_cover_every_uncomputed_file() {
        assert!(
            constant_content(SynthFile::Filesystems)
                .unwrap()
                .windows(4)
                .any(|w| w == b"ext4")
        );
        assert!(constant_content(SynthFile::Diskstats).unwrap().is_empty());
        assert_eq!(constant_content(SynthFile::Stat), None);
        assert_eq!(constant_content(SynthFile::SysHostname), None);
        assert_eq!(
            constant_content(SynthFile::SysPidMax),
            Some(&b"4194304\n"[..])
        );
    }

    #[test]
    fn cpu_lists_expand_ranges() {
        assert_eq!(
            cpu_list(b"0-7\n").collect::<Vec<_>>(),
            (0..8).collect::<Vec<_>>()
        );
        assert_eq!(cpu_list(b"0,2-3,9").collect::<Vec<_>>(), vec![0, 2, 3, 9]);
        assert_eq!(cpu_list(b"5").collect::<Vec<_>>(), vec![5]);
        assert_eq!(cpu_list(b"").count(), 0);
        assert_eq!(cpu_list(b"x").count(), 0);
        assert_eq!(cpu_list(b"3-1").count(), 0);
    }

    #[test]
    fn reads_the_tracer_field() {
        let status = b"Name:\tstrace\nUmask:\t0022\nState:\tt (tracing stop)\nTgid:\t16259\n\
            Ngid:\t0\nPid:\t16259\nPPid:\t16258\nTracerPid:\t16256\nUid:\t10368\t10368\n";
        assert_eq!(tracer_pid(status), Some(16256));
        assert_eq!(
            tracer_pid(b"Name:\tbash\nTracerPid:\t0\nUid:\t0\n"),
            Some(0)
        );
        assert_eq!(tracer_pid(b"TracerPid: 7"), Some(7));
    }

    #[test]
    fn missing_or_empty_field_is_none() {
        assert_eq!(tracer_pid(b"Name:\tbash\nUid:\t0\n"), None);
        assert_eq!(tracer_pid(b"TracerPid:\t\n"), None);
        assert_eq!(tracer_pid(b""), None);
    }
}
