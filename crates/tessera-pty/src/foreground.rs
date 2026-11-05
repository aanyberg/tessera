//! The foreground job of a PTY: which process group owns the terminal, and
//! its name, so a frontend can ask before closing a session that is busy
//! (issue #34, ADR 0031).
//!
//! A shell with job control (every interactive shell on a terminal) puts
//! each job it starts in a process group of its own and makes that group
//! the terminal's foreground group (`tcsetpgrp(3)`); at its prompt it takes
//! the terminal back. So the session is busy exactly when the foreground
//! group, read from the master with `tcgetpgrp(3)`, is not the shell's own
//! group, whose id is the shell's pid (the shell is the session leader).
//!
//! The process name is the group leader's: `proc_name` on macOS (at most
//! 31 bytes, as the kernel stores it), falling back to the 16-byte command name for a process of
//! another user (`sudo`, whose `proc_name` the kernel refuses), and
//! `/proc/<pid>/comm` on Linux. All `unsafe` is confined to the macOS
//! lookups below.

use portable_pty::MasterPty;

/// Most bytes [`ForegroundProcess::name`] holds. Longer names are cut at
/// the last whole character that fits.
pub const MAX_FOREGROUND_NAME_BYTES: usize = 64;

/// A job running in the foreground of a session's terminal instead of its
/// shell (see [`Pty::foreground_process`](crate::Pty::foreground_process)).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForegroundProcess {
    /// The terminal's foreground process group, which is the pid of the
    /// group's leader (the first process of the job, such as `vim`, or
    /// `cat` in `cat | less`). The leader may have exited while the rest
    /// of its group runs on.
    pub pid: u32,
    /// The group leader's name, lossily decoded as UTF-8 and at most
    /// [`MAX_FOREGROUND_NAME_BYTES`] bytes long. Empty when the name cannot be read:
    /// the leader has exited while the rest of its group runs on, or the
    /// platform offers no lookup.
    pub name: String,
}

/// The foreground job on `master`, whose session leader is the shell with
/// pid `shell`, or `None` when the shell's own group owns the terminal or
/// the master reports no foreground group.
///
/// The caller must keep `shell` from being reaped while this runs, so its
/// pid cannot be recycled into the comparison.
pub(crate) fn foreground_process(master: &dyn MasterPty, shell: u32) -> Option<ForegroundProcess> {
    let group = u32::try_from(master.process_group_leader()?).ok()?;
    (group != shell).then(|| ForegroundProcess {
        pid: group,
        name: process_name(group).unwrap_or_default(),
    })
}

/// The name of process `pid`, or `None` if it cannot be read.
fn process_name(pid: u32) -> Option<String> {
    let pid = i32::try_from(pid).ok().filter(|&pid| pid > 0)?;
    platform::process_name(pid).map(|bytes| truncated_name(&bytes))
}

/// `bytes` decoded lossily as UTF-8 and cut to at most
/// [`MAX_FOREGROUND_NAME_BYTES`] bytes at a character boundary. An
/// incomplete UTF-8 sequence at the end is dropped first: the kernel cuts
/// names at a byte count (31 bytes on macOS, 15 on Linux), which can split
/// the last character, and that piece would otherwise decode as a trailing
/// U+FFFD (#203).
fn truncated_name(bytes: &[u8]) -> String {
    let mut name = String::from_utf8_lossy(without_cut_character(bytes)).into_owned();
    let mut end = name.len().min(MAX_FOREGROUND_NAME_BYTES);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name.truncate(end);
    name
}

/// `bytes` without an incomplete UTF-8 sequence at its end: a lead byte
/// followed by fewer continuation bytes than it announces. Invalid bytes
/// are kept (they decode as U+FFFD), and so is everything else.
fn without_cut_character(bytes: &[u8]) -> &[u8] {
    // An incomplete sequence is at most three bytes (of a four-byte
    // character), so its lead byte is one of the last three.
    let tail = bytes.len().saturating_sub(3);
    let Some(lead) = (tail..bytes.len()).rev().find(|&i| bytes[i] & 0xC0 != 0x80) else {
        return bytes;
    };
    match std::str::from_utf8(&bytes[lead..]) {
        // `error_len` is `None` exactly when the input ends inside a
        // sequence that is valid so far.
        Err(error) if error.error_len().is_none() => &bytes[..lead],
        _ => bytes,
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_int;

    /// The kernel's process name (`p_name`, set from the executable's file
    /// name, at most `2 * MAXCOMLEN` bytes), or, when the kernel refuses it
    /// (a process of another user, such as `sudo`), the command name
    /// (`p_comm`, at most `MAXCOMLEN` bytes), which it reports for any
    /// process.
    pub(super) fn process_name(pid: c_int) -> Option<Vec<u8>> {
        long_name(pid).or_else(|| short_name(pid))
    }

    fn long_name(pid: c_int) -> Option<Vec<u8>> {
        // `proc_name` copies at most `2 * MAXCOMLEN` bytes plus a NUL.
        let mut buffer = [0u8; 2 * libc::MAXCOMLEN + 1];
        let size = u32::try_from(buffer.len()).expect("the buffer is 33 bytes");
        // SAFETY: `buffer` is writable for `size` bytes and outlives the
        // call. libproc's `proc_name` copies the 32-byte `pbi_name` field
        // into it and then `strlen`s the buffer to compute its result; a
        // 32-byte name would carry no NUL of its own, so the zeroed 33rd
        // byte is what keeps that `strlen` inside the buffer. It returns
        // the name's length (0 on failure, when only that length is read,
        // never the contents).
        let written = unsafe { libc::proc_name(pid, buffer.as_mut_ptr().cast(), size) };
        let written = usize::try_from(written).ok().filter(|&n| n > 0)?;
        let name = &buffer[..written.min(buffer.len())];
        Some(until_nul(name).to_vec())
    }

    fn short_name(pid: c_int) -> Option<Vec<u8>> {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
        let size = c_int::try_from(size_of::<libc::proc_bsdshortinfo>())
            .expect("proc_bsdshortinfo is 64 bytes");
        // SAFETY: `info` is writable for `size` bytes, the size of the
        // structure `PROC_PIDT_SHORTBSDINFO` fills, and outlives the call.
        // `proc_pidinfo` returns the bytes it wrote, or 0 or less on
        // failure.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDT_SHORTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if written != size {
            return None;
        }
        // SAFETY: the kernel filled the whole structure (`written ==
        // size`), and it was zeroed before, so every field is initialised;
        // all of its fields are plain integers.
        let info = unsafe { info.assume_init() };
        let comm: Vec<u8> = info.pbsi_comm.iter().map(|&c| c.cast_unsigned()).collect();
        let name = until_nul(&comm);
        (!name.is_empty()).then(|| name.to_vec())
    }

    /// `bytes` up to its first NUL.
    fn until_nul(bytes: &[u8]) -> &[u8] {
        bytes
            .iter()
            .position(|&b| b == 0)
            .map_or(bytes, |end| &bytes[..end])
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_process_of_another_user_is_named_by_its_command() {
            // launchd runs as root: the kernel refuses its `proc_name` to
            // an ordinary user and still reports its command name.
            assert_eq!(process_name(1).as_deref(), Some(&b"launchd"[..]));
        }

        #[test]
        fn this_process_has_a_long_name() {
            let pid = c_int::try_from(std::process::id()).unwrap();
            let name = long_name(pid).expect("our own name is readable");
            assert!(!name.is_empty() && !name.contains(&0), "{name:?}");
            // A test binary's name (`tessera_pty-<hash>`) is longer than
            // the 16-byte command name: the lookup keeps all of it.
            assert!(name.len() > libc::MAXCOMLEN, "{name:?}");
            assert_eq!(process_name(pid), Some(name));
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    /// `/proc/<pid>/comm` without its newline (at most 15 bytes).
    pub(super) fn process_name(pid: i32) -> Option<Vec<u8>> {
        let mut name = std::fs::read(format!("/proc/{pid}/comm")).ok()?;
        if name.last() == Some(&b'\n') {
            name.pop();
        }
        Some(name)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    /// No lookup on this platform.
    pub(super) fn process_name(_pid: i32) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cut_at_a_character_boundary() {
        assert_eq!(truncated_name(b"sleep"), "sleep");
        let long = "a".repeat(100);
        assert_eq!(truncated_name(long.as_bytes()), "a".repeat(64));
        // 63 ASCII bytes and a 3-byte character straddle the limit.
        let straddling = format!("{}\u{20ac}", "b".repeat(63));
        assert_eq!(truncated_name(straddling.as_bytes()), "b".repeat(63));
        let exact = format!("{}\u{e9}", "c".repeat(62));
        assert_eq!(truncated_name(exact.as_bytes()), exact);
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        assert_eq!(truncated_name(b"vi\xffm"), "vi\u{fffd}m");
        // A replacement character never pushes the name over the limit.
        let name = truncated_name(&[0xff; 64]);
        assert!(name.len() <= MAX_FOREGROUND_NAME_BYTES, "{}", name.len());
        assert_eq!(name, "\u{fffd}".repeat(21));
    }

    /// #203 items 8 and 9: an executable named `a` and eight emoji (33
    /// bytes) is cut by the macOS kernel to 31 bytes, inside the eighth
    /// emoji; the name decodes to the seven whole ones, with no trailing
    /// U+FFFD. Every cut of a multi-byte character is dropped, invalid
    /// bytes still decode as U+FFFD, and a whole last character is kept.
    #[test]
    fn a_character_cut_by_the_kernel_is_dropped() {
        let full = format!("a{}", "\u{1f600}".repeat(8));
        let cut = &full.as_bytes()[..31];
        assert_eq!(truncated_name(cut), format!("a{}", "\u{1f600}".repeat(7)));
        for (character, keep) in [("\u{e9}", 1), ("\u{20ac}", 2), ("\u{1f600}", 3)] {
            for piece in 1..=keep {
                let bytes = [b"vim".as_slice(), &character.as_bytes()[..piece]].concat();
                assert_eq!(truncated_name(&bytes), "vim", "{character} cut at {piece}");
            }
            let whole = format!("vim{character}");
            assert_eq!(truncated_name(whole.as_bytes()), whole);
        }
        assert_eq!(truncated_name(b"vi\xff"), "vi\u{fffd}");
        assert_eq!(truncated_name(b"vi\xf0\x9f\x98\xff"), "vi\u{fffd}\u{fffd}");
        assert_eq!(truncated_name(b"\x9f\x98"), "\u{fffd}\u{fffd}");
        assert_eq!(truncated_name(b""), "");
    }

    #[test]
    fn unreadable_pids_have_no_name() {
        assert_eq!(process_name(0), None);
        assert_eq!(process_name(u32::MAX), None);
    }
}
