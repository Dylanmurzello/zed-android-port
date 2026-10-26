//! The identity a fake-root guest believes it has.
//!
//! The kernel identity of an engine process never changes: it is the
//! app's uid, and nothing in an untrusted app can switch it. Programs do
//! not know that. They call `setuid` and friends and then read the ids
//! back to check that the switch happened: apt's transport methods drop
//! to `_apt` and refuse to run while `getgid()` still says root, `su`
//! and daemons do the same. So the engine keeps this record per process,
//! applies the kernel's own rules (`kernel/sys.c`) to it, and answers
//! the `get*` family from it. Across `execve` the record travels in the
//! per-exec variable [`crate::env::IDENTITY`].

pub const MAX_GROUPS: usize = 64;

/// Longest [`Identity::encode`] output: eleven bytes per id.
pub const MAX_ENCODED: usize = 11 * (8 + MAX_GROUPS);

/// `(uid_t) -1`, the kernel's "leave this one unchanged".
pub const UNCHANGED: u32 = u32::MAX;

const EPERM: i32 = 1;
const EINVAL: i32 = 22;

const S_ISUID: u32 = 0o4000;
const S_ISGID: u32 = 0o2000;
const S_IXGRP: u32 = 0o010;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub ruid: u32,
    pub euid: u32,
    pub suid: u32,
    pub fsuid: u32,
    pub rgid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub fsgid: u32,
    groups: [u32; MAX_GROUPS],
    ngroups: usize,
}

impl Identity {
    pub const ROOT: Self = Self {
        ruid: 0,
        euid: 0,
        suid: 0,
        fsuid: 0,
        rgid: 0,
        egid: 0,
        sgid: 0,
        fsgid: 0,
        groups: [0; MAX_GROUPS],
        ngroups: 0,
    };

    /// Stand-in for `CAP_SETUID`/`CAP_SETGID`. The kernel clears the
    /// effective capabilities when the effective uid leaves 0 and
    /// restores them when it returns, and clears the permitted set only
    /// when every uid leaves 0, after which 0 is no longer reachable by
    /// the unprivileged rules below. "Effective root" is therefore
    /// exactly the privileged state for this family.
    fn privileged(&self) -> bool {
        self.euid == 0
    }

    pub fn may_setgroups(&self) -> bool {
        self.privileged()
    }

    pub fn groups(&self) -> &[u32] {
        &self.groups[..self.ngroups]
    }

    pub fn setuid(&mut self, uid: u32) -> Result<(), i32> {
        if self.privileged() {
            self.ruid = uid;
            self.suid = uid;
        } else if uid != self.ruid && uid != self.suid {
            return Err(EPERM);
        }
        self.euid = uid;
        self.fsuid = uid;
        Ok(())
    }

    pub fn setgid(&mut self, gid: u32) -> Result<(), i32> {
        if self.privileged() {
            self.rgid = gid;
            self.sgid = gid;
        } else if gid != self.rgid && gid != self.sgid {
            return Err(EPERM);
        }
        self.egid = gid;
        self.fsgid = gid;
        Ok(())
    }

    pub fn setreuid(&mut self, ruid: u32, euid: u32) -> Result<(), i32> {
        let (old_r, old_e, old_s) = (self.ruid, self.euid, self.suid);
        let privileged = self.privileged();
        if ruid != UNCHANGED && !privileged && ruid != old_r && ruid != old_e {
            return Err(EPERM);
        }
        if euid != UNCHANGED && !privileged && euid != old_r && euid != old_e && euid != old_s {
            return Err(EPERM);
        }
        if ruid != UNCHANGED {
            self.ruid = ruid;
        }
        if euid != UNCHANGED {
            self.euid = euid;
        }
        if ruid != UNCHANGED || (euid != UNCHANGED && euid != old_r) {
            self.suid = self.euid;
        }
        self.fsuid = self.euid;
        Ok(())
    }

    pub fn setregid(&mut self, rgid: u32, egid: u32) -> Result<(), i32> {
        let (old_r, old_e, old_s) = (self.rgid, self.egid, self.sgid);
        let privileged = self.privileged();
        if rgid != UNCHANGED && !privileged && rgid != old_r && rgid != old_e {
            return Err(EPERM);
        }
        if egid != UNCHANGED && !privileged && egid != old_r && egid != old_e && egid != old_s {
            return Err(EPERM);
        }
        if rgid != UNCHANGED {
            self.rgid = rgid;
        }
        if egid != UNCHANGED {
            self.egid = egid;
        }
        if rgid != UNCHANGED || (egid != UNCHANGED && egid != old_r) {
            self.sgid = self.egid;
        }
        self.fsgid = self.egid;
        Ok(())
    }

    pub fn setresuid(&mut self, ruid: u32, euid: u32, suid: u32) -> Result<(), i32> {
        if !self.privileged() {
            for id in [ruid, euid, suid] {
                if id != UNCHANGED && id != self.ruid && id != self.euid && id != self.suid {
                    return Err(EPERM);
                }
            }
        }
        if ruid != UNCHANGED {
            self.ruid = ruid;
        }
        if euid != UNCHANGED {
            self.euid = euid;
        }
        if suid != UNCHANGED {
            self.suid = suid;
        }
        self.fsuid = self.euid;
        Ok(())
    }

    pub fn setresgid(&mut self, rgid: u32, egid: u32, sgid: u32) -> Result<(), i32> {
        if !self.privileged() {
            for id in [rgid, egid, sgid] {
                if id != UNCHANGED && id != self.rgid && id != self.egid && id != self.sgid {
                    return Err(EPERM);
                }
            }
        }
        if rgid != UNCHANGED {
            self.rgid = rgid;
        }
        if egid != UNCHANGED {
            self.egid = egid;
        }
        if sgid != UNCHANGED {
            self.sgid = sgid;
        }
        self.fsgid = self.egid;
        Ok(())
    }

    /// Returns the previous value, like the syscall, whether or not the
    /// change was allowed.
    pub fn setfsuid(&mut self, fsuid: u32) -> u32 {
        let old = self.fsuid;
        if self.privileged() || [self.ruid, self.euid, self.suid, self.fsuid].contains(&fsuid) {
            self.fsuid = fsuid;
        }
        old
    }

    pub fn setfsgid(&mut self, fsgid: u32) -> u32 {
        let old = self.fsgid;
        if self.privileged() || [self.rgid, self.egid, self.sgid, self.fsgid].contains(&fsgid) {
            self.fsgid = fsgid;
        }
        old
    }

    pub fn setgroups(&mut self, list: &[u32]) -> Result<(), i32> {
        if !self.may_setgroups() {
            return Err(EPERM);
        }
        if list.len() > MAX_GROUPS {
            return Err(EINVAL);
        }
        self.groups[..list.len()].copy_from_slice(list);
        self.ngroups = list.len();
        Ok(())
    }

    /// What `execve` does to the identity (`bprm_fill_uid` and
    /// `cap_bprm_creds_from_file`): a set-id file lends its owner's id
    /// to the effective slot, and every exec copies the effective ids
    /// into the saved and filesystem ones.
    pub fn exec_setid(&mut self, mode: u32, owner_uid: u32, owner_gid: u32) {
        if mode & S_ISUID != 0 {
            self.euid = owner_uid;
        }
        if mode & (S_ISGID | S_IXGRP) == S_ISGID | S_IXGRP {
            self.egid = owner_gid;
        }
        self.suid = self.euid;
        self.fsuid = self.euid;
        self.sgid = self.egid;
        self.fsgid = self.egid;
    }

    /// `ruid,euid,suid,fsuid,rgid,egid,sgid,fsgid[,group...]` in decimal.
    /// Returns the length written; `out` must hold [`MAX_ENCODED`].
    pub fn encode(&self, out: &mut [u8; MAX_ENCODED]) -> usize {
        let mut len = 0usize;
        let ids = [
            self.ruid, self.euid, self.suid, self.fsuid, self.rgid, self.egid, self.sgid,
            self.fsgid,
        ];
        for (index, value) in ids.iter().chain(self.groups().iter()).enumerate() {
            if index > 0 {
                out[len] = b',';
                len += 1;
            }
            len += push_decimal(*value, &mut out[len..]);
        }
        len
    }

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let mut identity = Self::ROOT;
        let mut fields = bytes.split(|&byte| byte == b',');
        let mut ids = [0u32; 8];
        for slot in &mut ids {
            *slot = parse_decimal(fields.next()?)?;
        }
        [
            identity.ruid,
            identity.euid,
            identity.suid,
            identity.fsuid,
            identity.rgid,
            identity.egid,
            identity.sgid,
            identity.fsgid,
        ] = ids;
        for field in fields {
            if identity.ngroups == MAX_GROUPS {
                return None;
            }
            identity.groups[identity.ngroups] = parse_decimal(field)?;
            identity.ngroups += 1;
        }
        Some(identity)
    }
}

fn push_decimal(value: u32, out: &mut [u8]) -> usize {
    let mut digits = [0u8; 10];
    let mut count = 0usize;
    let mut rest = value;
    loop {
        digits[count] = b'0' + (rest % 10) as u8;
        count += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    for (index, digit) in digits[..count].iter().rev().enumerate() {
        out[index] = *digit;
    }
    count
}

fn parse_decimal(field: &[u8]) -> Option<u32> {
    if field.is_empty() || field.len() > 10 {
        return None;
    }
    let mut value = 0u32;
    for &byte in field {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: u32) -> Identity {
        let mut identity = Identity::ROOT;
        identity.setgid(id).unwrap();
        identity.setuid(id).unwrap();
        identity
    }

    #[test]
    fn root_drops_everything_with_setuid() {
        let identity = user(1000);
        assert_eq!(
            (identity.ruid, identity.euid, identity.suid, identity.fsuid),
            (1000, 1000, 1000, 1000)
        );
        assert_eq!((identity.rgid, identity.egid, identity.sgid, identity.fsgid), (1000, 1000, 1000, 1000));
    }

    #[test]
    fn apt_privilege_drop_verifies() {
        // apt-pkg/contrib/fileutl.cc DropPrivileges, as glibc issues it.
        let apt = 42u32;
        let mut identity = Identity::ROOT;
        identity.setgroups(&[apt]).unwrap();
        identity.setresgid(apt, apt, apt).unwrap();
        identity.setresgid(UNCHANGED, apt, UNCHANGED).unwrap(); // setegid
        identity.setgid(apt).unwrap();
        identity.setresuid(apt, apt, apt).unwrap();
        identity.setuid(apt).unwrap();
        identity.setresuid(UNCHANGED, apt, UNCHANGED).unwrap(); // seteuid
        assert_eq!(identity.groups(), &[apt]);
        assert_eq!((identity.rgid, identity.egid), (apt, apt));
        assert_eq!((identity.ruid, identity.euid), (apt, apt));
        // "still able to switch back" must be false.
        assert_eq!(identity.setuid(0), Err(EPERM));
        assert_eq!(identity.setgid(0), Err(EPERM));
        assert_eq!(identity.setgroups(&[0]), Err(EPERM));
        assert_eq!(identity.setresuid(UNCHANGED, 0, UNCHANGED), Err(EPERM));
    }

    #[test]
    fn temporary_drop_returns_to_root() {
        let mut identity = Identity::ROOT;
        identity.setresuid(UNCHANGED, 1000, UNCHANGED).unwrap();
        assert_eq!((identity.ruid, identity.euid, identity.suid), (0, 1000, 0));
        assert!(!identity.privileged());
        assert_eq!(identity.setuid(2000), Err(EPERM));
        identity.setresuid(UNCHANGED, 0, UNCHANGED).unwrap();
        assert!(identity.privileged());
        assert_eq!((identity.ruid, identity.euid, identity.suid), (0, 0, 0));
    }

    #[test]
    fn unprivileged_setuid_reaches_only_real_and_saved() {
        let mut identity = Identity::ROOT;
        identity.setresuid(1000, 1000, 0).unwrap();
        identity.setuid(0).unwrap(); // saved uid is 0
        assert_eq!(identity.euid, 0);
        let mut identity = user(1000);
        assert_eq!(identity.setuid(1001), Err(EPERM));
        assert_eq!(identity.setreuid(1001, UNCHANGED), Err(EPERM));
        identity.setreuid(UNCHANGED, 1000).unwrap();
    }

    #[test]
    fn setreuid_updates_saved_like_the_kernel() {
        let mut identity = Identity::ROOT;
        identity.setreuid(1000, 1000).unwrap();
        assert_eq!((identity.ruid, identity.euid, identity.suid), (1000, 1000, 1000));
        let mut identity = Identity::ROOT;
        identity.setreuid(UNCHANGED, 1000).unwrap();
        assert_eq!((identity.ruid, identity.euid, identity.suid), (0, 1000, 1000));
        let mut identity = Identity::ROOT;
        identity.setreuid(UNCHANGED, 0).unwrap();
        assert_eq!(identity.suid, 0);
    }

    #[test]
    fn setfsuid_reports_the_previous_value() {
        let mut identity = user(1000);
        assert_eq!(identity.setfsuid(2000), 1000);
        assert_eq!(identity.fsuid, 1000);
        assert_eq!(identity.setfsuid(1000), 1000);
        let mut root = Identity::ROOT;
        assert_eq!(root.setfsuid(2000), 0);
        assert_eq!(root.fsuid, 2000);
    }

    #[test]
    fn setgroups_bounds() {
        let mut identity = Identity::ROOT;
        assert_eq!(identity.setgroups(&[0u32; MAX_GROUPS + 1]), Err(EINVAL));
        identity.setgroups(&[5, 6, 7]).unwrap();
        assert_eq!(identity.groups(), &[5, 6, 7]);
        identity.setgroups(&[]).unwrap();
        assert!(identity.groups().is_empty());
    }

    #[test]
    fn exec_of_a_setuid_file_lends_its_owner() {
        let mut identity = user(1000);
        identity.exec_setid(0o4755, 0, 0);
        assert_eq!((identity.ruid, identity.euid, identity.suid, identity.fsuid), (1000, 0, 0, 0));
        assert_eq!(identity.egid, 1000);
        let mut identity = user(1000);
        identity.exec_setid(0o2755, 0, 0);
        assert_eq!(identity.euid, 1000);
        assert_eq!((identity.egid, identity.sgid), (0, 0));
        // Set-gid without group execute is mandatory locking, not set-gid.
        let mut identity = user(1000);
        identity.exec_setid(0o2745, 0, 0);
        assert_eq!(identity.egid, 1000);
        // A plain exec copies the effective ids into the saved ones.
        let mut identity = Identity::ROOT;
        identity.setresuid(UNCHANGED, 1000, UNCHANGED).unwrap();
        identity.exec_setid(0o755, 0, 0);
        assert_eq!((identity.ruid, identity.euid, identity.suid), (0, 1000, 1000));
    }

    #[test]
    fn encode_parse_round_trip() {
        let mut identity = Identity::ROOT;
        identity.setgroups(&[4, 24, u32::MAX - 1]).unwrap();
        identity.setresgid(100, 101, 102).unwrap();
        identity.setfsgid(103);
        identity.setresuid(1, 2, 3).unwrap();
        let mut out = [0u8; MAX_ENCODED];
        let len = identity.encode(&mut out);
        assert_eq!(&out[..len], b"1,2,3,2,100,101,102,103,4,24,4294967294");
        assert_eq!(Identity::parse(&out[..len]), Some(identity));
        let mut out = [0u8; MAX_ENCODED];
        let len = Identity::ROOT.encode(&mut out);
        assert_eq!(&out[..len], b"0,0,0,0,0,0,0,0");
        assert_eq!(Identity::parse(b"0,0,0,0,0,0,0,0"), Some(Identity::ROOT));
    }

    #[test]
    fn parse_rejects_malformed_values() {
        assert_eq!(Identity::parse(b""), None);
        assert_eq!(Identity::parse(b"0,0,0,0,0,0,0"), None);
        assert_eq!(Identity::parse(b"0,0,0,0,0,0,0,0,"), None);
        assert_eq!(Identity::parse(b"0,0,0,0,0,0,0,x"), None);
        assert_eq!(Identity::parse(b"4294967296,0,0,0,0,0,0,0"), None);
        let mut full = Identity::ROOT;
        full.setgroups(&[1; MAX_GROUPS]).unwrap();
        let mut out = [0u8; MAX_ENCODED];
        let len = full.encode(&mut out);
        assert_eq!(Identity::parse(&out[..len]), Some(full));
        let mut too_many = [0u8; MAX_ENCODED + 2];
        too_many[..len].copy_from_slice(&out[..len]);
        too_many[len..len + 2].copy_from_slice(b",1");
        assert_eq!(Identity::parse(&too_many[..len + 2]), None);
    }
}
