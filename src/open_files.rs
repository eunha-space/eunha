//! The process's limit on open files.
//!
//! A service launchd starts has a soft limit of 256 open files unless its
//! plist sets another, and every socket counts: each delivery in flight, each
//! connection kept for reuse, each database and Redis connection. Delivering
//! 128 at a time to a few hundred servers passes 256, and past it nothing
//! opens — a database connection no more than a delivery. The hard limit is
//! far higher, and a process may raise its own soft limit up to it, so eunha
//! does, rather than depend on every deployment's service definition.

/// Below this, the limit is low for a process delivering a fan-out.
pub const COMFORTABLE: u64 = 4096;

/// Raise the soft limit on open files as far as the system allows. Returns the
/// limit before and after.
///
/// # Errors
///
/// When the limit cannot be read or set.
pub fn raise() -> std::io::Result<(u64, u64)> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes into the struct it is given, which lives for
    // the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let before = limit.rlim_cur;
    let target = system_cap(limit.rlim_max);
    if target <= before {
        return Ok((before, before));
    }
    limit.rlim_cur = target;
    // SAFETY: as above; the struct is only read.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((before, target))
}

/// The most a soft limit may be set to. macOS refuses one above
/// `kern.maxfilesperproc`, including the "unlimited" its hard limit usually is.
#[cfg(target_os = "macos")]
fn system_cap(hard: libc::rlim_t) -> libc::rlim_t {
    let mut max: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is a NUL-terminated string, and `max` and `size`
    // describe a buffer that lives for the call.
    let read = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&mut max as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read == 0 && max > 0 {
        hard.min(max as libc::rlim_t)
    } else {
        hard
    }
}

/// The most a soft limit may be set to: elsewhere, the hard limit.
#[cfg(not(target_os = "macos"))]
fn system_cap(hard: libc::rlim_t) -> libc::rlim_t {
    hard
}

#[cfg(test)]
mod tests {
    #[test]
    fn raising_never_lowers_the_limit_and_sticks() {
        let (before, after) = super::raise().expect("the limit can be read and set");
        assert!(after >= before);
        let (again, _) = super::raise().expect("and read again");
        assert_eq!(again, after, "the raised limit is the one in force");
    }
}
