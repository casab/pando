//! Reading a line with the terminal's echo off.

/// A line from the terminal with nothing it types shown: a password is not
/// for the scrollback, or for whoever is looking at the screen.
///
/// The terminal's echo is switched off for the one read and back on after
/// it, newline included, so the next line of output starts where it
/// should. A stdin that is not a terminal has no echo to switch.
pub fn read_line_unechoed(line: &mut String) -> std::io::Result<usize> {
    imp::read_line_unechoed(line)
}

#[cfg(unix)]
mod imp {
    use std::os::unix::io::AsRawFd;

    pub(super) fn read_line_unechoed(line: &mut String) -> std::io::Result<usize> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: a zeroed termios is a valid value to be overwritten by
        // `tcgetattr`, which is all it is used for when that call fails.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is this process's stdin and `saved` is a termios.
        let is_terminal = unsafe { libc::tcgetattr(fd, &mut saved) } == 0;
        if is_terminal {
            let mut quiet = saved;
            quiet.c_lflag &= !libc::ECHO;
            quiet.c_lflag |= libc::ECHONL;
            // SAFETY: as above; `quiet` is `saved` with two flags changed.
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
        }
        let read = std::io::stdin().read_line(line);
        if is_terminal {
            // SAFETY: puts back exactly what `tcgetattr` returned.
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
        }
        read
    }
}
