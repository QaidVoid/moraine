//! Build isolation enforcement in the spawned phase child.
//!
//! The sandbox plan selects which Linux namespaces to unshare and whether to
//! drop to the unprivileged build user. This module carries that selection as
//! [`Isolation`] and applies it inside the forked child before `exec`, mirroring
//! `portage.process.spawn`: it unshares the requested namespaces, brings the
//! loopback interface up in a fresh network namespace, then drops gid,
//! supplementary groups, uid, and sets the umask. The build-user resolution runs
//! in the parent so the child performs only async-signal-safe syscalls.

use crate::sandbox::Namespace;

/// The privilege drop target for `userpriv`: the build user's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegeDrop {
    /// The build user's uid.
    pub uid: u32,
    /// The build user's primary gid.
    pub gid: u32,
    /// The supplementary groups to set.
    pub groups: Vec<u32>,
    /// The umask to install before exec.
    pub umask: u32,
}

/// The isolation a phase child must enforce: the namespaces to unshare and an
/// optional privilege drop.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Isolation {
    /// The Linux namespaces to unshare in the child.
    pub namespaces: Vec<Namespace>,
    /// The build-user privilege drop, when `userpriv` is enforced.
    pub privilege: Option<PrivilegeDrop>,
}

impl Isolation {
    /// Whether there is nothing to enforce.
    pub fn is_empty(&self) -> bool {
        self.namespaces.is_empty() && self.privilege.is_none()
    }

    /// The FEATURES tokens this isolation enforces: each namespace's feature name
    /// plus `userpriv` when a privilege drop is present.
    pub fn applied_tokens(&self) -> Vec<String> {
        let mut tokens: Vec<String> = self
            .namespaces
            .iter()
            .map(|ns| ns.feature().to_string())
            .collect();
        if self.privilege.is_some() {
            tokens.push("userpriv".to_string());
        }
        tokens
    }
}

/// Resolve the build user's privilege drop target from the local `/etc/passwd`
/// and `/etc/group`, mirroring `portage.data`.
///
/// `username` (`PORTAGE_USERNAME`) supplies the uid, `groupname`
/// (`PORTAGE_GRPNAME`) supplies the primary gid, and every group whose member
/// list contains `username` contributes a supplementary group. The umask is
/// `0o22`. Returns `None` when the user is not present in `/etc/passwd`. This
/// runs in the parent before spawn; richer NSS backends are out of scope.
pub fn resolve_build_user(username: &str, groupname: &str) -> Option<PrivilegeDrop> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    let (uid, primary_gid) = passwd_lookup(&passwd, username)?;

    let group = std::fs::read_to_string("/etc/group").unwrap_or_default();
    // The primary gid is the named group's gid when present, else the passwd gid.
    let gid = group_gid(&group, groupname).unwrap_or(primary_gid);

    let mut groups = vec![gid];
    for sup in supplementary_groups(&group, username) {
        if !groups.contains(&sup) {
            groups.push(sup);
        }
    }

    Some(PrivilegeDrop {
        uid,
        gid,
        groups,
        umask: 0o22,
    })
}

/// Parse `/etc/passwd` for `username`, returning its `(uid, gid)`.
fn passwd_lookup(passwd: &str, username: &str) -> Option<(u32, u32)> {
    for line in passwd.lines() {
        let mut fields = line.split(':');
        if fields.next() != Some(username) {
            continue;
        }
        let _passwd = fields.next();
        let uid = fields.next()?.parse().ok()?;
        let gid = fields.next()?.parse().ok()?;
        return Some((uid, gid));
    }
    None
}

/// Parse `/etc/group` for `groupname`, returning its gid.
fn group_gid(group: &str, groupname: &str) -> Option<u32> {
    for line in group.lines() {
        let mut fields = line.split(':');
        if fields.next() != Some(groupname) {
            continue;
        }
        let _passwd = fields.next();
        return fields.next()?.parse().ok();
    }
    None
}

/// The gids of every `/etc/group` entry whose member list contains `username`.
fn supplementary_groups(group: &str, username: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for line in group.lines() {
        let mut fields = line.split(':');
        let _name = fields.next();
        let _passwd = fields.next();
        let Some(gid) = fields.next().and_then(|g| g.parse::<u32>().ok()) else {
            continue;
        };
        let members = fields.next().unwrap_or("");
        if members.split(',').any(|m| m == username) {
            out.push(gid);
        }
    }
    out
}

/// Apply the isolation in the forked child before `exec`.
///
/// Unshares the requested namespaces, configures loopback when the network
/// namespace was unshared, then drops gid, supplementary groups, and uid (in
/// that order) and installs the umask. Restricted to async-signal-safe syscalls
/// with no heap allocation: the caller clones the [`Isolation`] into the
/// pre-exec closure beforehand and this iterates the pre-owned vectors.
#[cfg(target_os = "linux")]
pub fn apply_in_child(iso: &Isolation) -> std::io::Result<()> {
    use rustix::thread::UnshareFlags;

    let mut flags = UnshareFlags::empty();
    let mut network = false;
    for ns in &iso.namespaces {
        match ns {
            Namespace::Network => {
                flags |= UnshareFlags::NEWNET | UnshareFlags::NEWUTS;
                network = true;
            }
            Namespace::Mount => flags |= UnshareFlags::NEWNS,
            Namespace::Pid => flags |= UnshareFlags::NEWPID | UnshareFlags::NEWNS,
            Namespace::Ipc => flags |= UnshareFlags::NEWIPC,
        }
    }

    if !flags.is_empty() {
        // `unshare_unsafe` is the non-deprecated entry point; the deprecated safe
        // wrapper only guards `CLONE_FILES`, which is not in our flag set.
        unsafe { rustix::thread::unshare_unsafe(flags) }.map_err(std::io::Error::from)?;
    }

    if network {
        configure_loopback()?;
    }

    if let Some(drop) = &iso.privilege {
        use rustix::process::{Gid, Uid};

        rustix::thread::set_thread_gid(Gid::from_raw(drop.gid)).map_err(std::io::Error::from)?;
        // `Gid` is `repr(transparent)` over the raw gid, so the pre-owned `u32`
        // slice can be reinterpreted without allocating a `Vec<Gid>`.
        let groups: &[Gid] = unsafe {
            std::slice::from_raw_parts(drop.groups.as_ptr() as *const Gid, drop.groups.len())
        };
        rustix::thread::set_thread_groups(groups).map_err(std::io::Error::from)?;
        rustix::thread::set_thread_uid(Uid::from_raw(drop.uid)).map_err(std::io::Error::from)?;
        rustix::process::umask(rustix::fs::Mode::from_raw_mode(drop.umask));
    }

    Ok(())
}

/// Bring the loopback interface up in a freshly unshared network namespace so
/// localhost still resolves, by setting `IFF_UP | IFF_RUNNING` on `lo` via
/// `SIOCSIFFLAGS`. A failure here is surfaced as a failed network isolation.
#[cfg(target_os = "linux")]
fn configure_loopback() -> std::io::Result<()> {
    use rustix::net::{AddressFamily, SocketType};

    let sock = rustix::net::socket(AddressFamily::INET, SocketType::DGRAM, None)
        .map_err(std::io::Error::from)?;

    // IFF_UP | IFF_RUNNING.
    const IFF_UP: i16 = 0x1;
    const IFF_RUNNING: i16 = 0x40;
    let mut name = [0u8; 16];
    name[0] = b'l';
    name[1] = b'o';
    let req = SetLoopbackFlags {
        req: Ifreq {
            name,
            flags: IFF_UP | IFF_RUNNING,
            _pad: [0u8; 22],
        },
    };
    unsafe { rustix::ioctl::ioctl(&sock, req) }.map_err(std::io::Error::from)
}

/// The kernel `struct ifreq` layout used for `SIOCSIFFLAGS` on `lo`.
#[cfg(target_os = "linux")]
#[repr(C, align(8))]
struct Ifreq {
    name: [u8; 16],
    flags: i16,
    _pad: [u8; 22],
}

/// A `SIOCSIFFLAGS` ioctl that sets the interface flags from its [`Ifreq`].
#[cfg(target_os = "linux")]
struct SetLoopbackFlags {
    req: Ifreq,
}

#[cfg(target_os = "linux")]
unsafe impl rustix::ioctl::Ioctl for SetLoopbackFlags {
    type Output = ();

    const IS_MUTATING: bool = false;

    fn opcode(&self) -> rustix::ioctl::Opcode {
        // SIOCSIFFLAGS.
        0x8914 as rustix::ioctl::Opcode
    }

    fn as_ptr(&mut self) -> *mut std::ffi::c_void {
        &mut self.req as *mut Ifreq as *mut std::ffi::c_void
    }

    unsafe fn output_from_ptr(
        _out: rustix::ioctl::IoctlOutput,
        _extract_output: *mut std::ffi::c_void,
    ) -> rustix::io::Result<Self::Output> {
        Ok(())
    }
}

/// Apply the isolation in the forked child. This non-Linux stub is a no-op; the
/// namespace and privilege primitives are Linux-specific.
#[cfg(not(target_os = "linux"))]
pub fn apply_in_child(_iso: &Isolation) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applied_tokens_lists_namespaces_and_userpriv() {
        let iso = Isolation {
            namespaces: vec![Namespace::Network, Namespace::Mount],
            privilege: Some(PrivilegeDrop {
                uid: 250,
                gid: 250,
                groups: vec![250],
                umask: 0o22,
            }),
        };
        let tokens = iso.applied_tokens();
        assert!(tokens.contains(&"network-sandbox".to_string()));
        assert!(tokens.contains(&"mount-sandbox".to_string()));
        assert!(tokens.contains(&"userpriv".to_string()));
        assert!(!iso.is_empty());
    }

    #[test]
    fn empty_isolation_has_no_tokens() {
        let iso = Isolation::default();
        assert!(iso.is_empty());
        assert!(iso.applied_tokens().is_empty());
    }

    #[test]
    fn resolves_user_from_passwd_and_group_fixtures() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nportage:x:250:250:portage:/var/tmp/portage:/sbin/nologin\n";
        let (uid, gid) = passwd_lookup(passwd, "portage").unwrap();
        assert_eq!(uid, 250);
        assert_eq!(gid, 250);
        assert_eq!(passwd_lookup(passwd, "absent"), None);

        let group = "portage:x:250:\nwheel:x:10:root,portage\nusers:x:100:other\n";
        assert_eq!(group_gid(group, "portage"), Some(250));
        let sup = supplementary_groups(group, "portage");
        assert!(sup.contains(&10));
        assert!(!sup.contains(&100));
    }
}
