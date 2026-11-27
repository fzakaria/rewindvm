//! The limit on how many files `rewind` may have open at once.

use libc::rlim_t;

/// Where the kernel says how many files a process can have open at most,
/// whatever its limit: it hands out no descriptor at or above fs.nr_open.
const NR_OPEN_PATH: &str = "/proc/sys/fs/nr_open";

/// The soft limit raised to when the hard limit is RLIM_INFINITY: the
/// kernel's default fs.nr_open, 1024 * 1024. Some hosts set fs.nr_open
/// near 2^31, and a program that walks every descriptor up to its soft
/// limit, as some close all of them before exec, would take that long.
const UNLIMITED_CAP: rlim_t = 1 << 20;

/// Raises this process's soft limit on open files to its hard limit, as
/// Go's runtime does for its programs: a `rewind check` running many VMs
/// at once can need more than the usual soft limit of 1024.
///
/// A hard limit of RLIM_INFINITY raises the soft limit to fs.nr_open or
/// `UNLIMITED_CAP`, whichever is lower. The soft limit is never lowered
/// and the hard limit never changes. Failing to read or set the limit is
/// no reason to fail the command, so then the limit stays as it was.
pub(crate) fn raise_limit() {
    // The current soft and hard limits.
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes only the rlimit it is given.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return;
    }

    // What to raise the soft limit to, if anything.
    let nr_open = std::fs::read_to_string(NR_OPEN_PATH)
        .ok()
        .and_then(|text| text.trim().parse::<rlim_t>().ok());
    let Some(soft) = raised_soft_limit(limit.rlim_cur, limit.rlim_max, nr_open) else {
        return;
    };

    // Set the soft limit, and keep the old one if the kernel refuses. The
    // kernel refuses any hard limit above fs.nr_open, RLIM_INFINITY
    // included, so an unlimited hard limit keeps the old soft limit too.
    let raised = libc::rlimit {
        rlim_cur: soft,
        rlim_max: limit.rlim_max,
    };
    // SAFETY: setrlimit reads only the rlimit it is given.
    let _ = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) };
}

/// The soft limit on open files to raise `soft` to, given the hard limit
/// and the kernel's fs.nr_open if it could be read, or `None` to keep it.
fn raised_soft_limit(soft: rlim_t, hard: rlim_t, nr_open: Option<rlim_t>) -> Option<rlim_t> {
    // A finite hard limit is the target. RLIM_INFINITY is no number of
    // files, so the target is then the cap, or fs.nr_open when lower.
    let target = if hard == libc::RLIM_INFINITY {
        nr_open.map_or(UNLIMITED_CAP, |n| n.min(UNLIMITED_CAP))
    } else {
        hard
    };

    // A soft limit already at or above the target stays.
    if soft >= target {
        return None;
    }
    Some(target)
}

#[cfg(test)]
mod tests {
    // The decision of what to raise the soft limit to, on chosen limits,
    // and the raise itself in a child test process whose soft limit was
    // lowered first, so the test process keeps its own.
    use super::*;

    /// Set in the child test process that lowers its soft limit and raises
    /// it again.
    const ENV_CHILD: &str = "REWIND_TEST_OPEN_FILES_CHILD";

    /// The soft limit the child test process lowers itself to.
    const LOWERED: rlim_t = 64;

    /// This process's soft and hard limits on open files.
    fn limits() -> libc::rlimit {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit writes only the rlimit it is given.
        let got = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
        assert_eq!(got, 0, "getrlimit: {}", std::io::Error::last_os_error());
        limit
    }

    #[test]
    fn a_soft_limit_below_a_finite_hard_limit_rises_to_it() {
        // The soft limit goes to the hard limit, whatever fs.nr_open says,
        // as Go's runtime does.
        assert_eq!(
            raised_soft_limit(1024, 524_288, Some(1 << 20)),
            Some(524_288)
        );
        assert_eq!(raised_soft_limit(1024, 524_288, None), Some(524_288));
    }

    #[test]
    fn a_soft_limit_at_or_above_the_target_stays() {
        // Nothing is lowered, and a soft limit already at the hard limit
        // needs no call.
        assert_eq!(raised_soft_limit(4096, 4096, Some(1 << 20)), None);
        assert_eq!(
            raised_soft_limit(2 * UNLIMITED_CAP, libc::RLIM_INFINITY, None),
            None
        );
    }

    #[test]
    fn an_unlimited_hard_limit_caps_the_soft_limit() {
        // RLIM_INFINITY is no number of files: the soft limit goes to
        // fs.nr_open or the cap, whichever is lower, and to the cap when
        // fs.nr_open could not be read.
        let infinity = libc::RLIM_INFINITY;
        assert_eq!(
            raised_soft_limit(1024, infinity, Some(1 << 31)),
            Some(UNLIMITED_CAP)
        );
        assert_eq!(raised_soft_limit(1024, infinity, Some(8192)), Some(8192));
        assert_eq!(raised_soft_limit(1024, infinity, None), Some(UNLIMITED_CAP));
    }

    #[test]
    fn raising_the_limit_sets_the_soft_limit_to_the_hard_one() {
        // This test runs itself again as a child process, which lowers its
        // soft limit, raises it, and checks it reached the hard limit.
        if std::env::var_os(ENV_CHILD).is_none() {
            let exe = std::env::current_exe().unwrap();
            let output = std::process::Command::new(exe)
                .args([
                    "--exact",
                    "open_files::tests::raising_the_limit_sets_the_soft_limit_to_the_hard_one",
                    "--test-threads=1",
                ])
                .env(ENV_CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child test process: {}\n{}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        // In the child: lower the soft limit, when the hard limit leaves
        // room to.
        let before = limits();
        if before.rlim_max <= LOWERED {
            return;
        }
        let lowered = libc::rlimit {
            rlim_cur: LOWERED,
            rlim_max: before.rlim_max,
        };
        // SAFETY: setrlimit reads only the rlimit it is given.
        let set = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) };
        assert_eq!(set, 0, "setrlimit: {}", std::io::Error::last_os_error());

        // Raise it again, and check it reached the hard limit (or UNLIMITED_CAP
        // when the hard limit is RLIM_INFINITY), while the hard limit stays.
        raise_limit();
        let after = limits();
        assert_eq!(after.rlim_max, before.rlim_max);
        let expected = if before.rlim_max == libc::RLIM_INFINITY {
            UNLIMITED_CAP
        } else {
            before.rlim_max
        };
        assert_eq!(after.rlim_cur, expected);
    }
}
