//! Thin wrappers over the Linux system calls the host uses.

use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::io::RawFd;

pub fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) })?;
    Ok(())
}

pub fn close(fd: RawFd) {
    unsafe { libc::close(fd) };
}

/// Read into `buf`. `Ok(None)` means nothing is available right now.
pub fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<Option<usize>> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n >= 0 {
            return Ok(Some(n as usize));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return Ok(None),
            _ => return Err(e),
        }
    }
}

/// Write from `buf`. `Ok(None)` means the fd cannot take data right now.
pub fn write(fd: RawFd, buf: &[u8]) -> io::Result<Option<usize>> {
    loop {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n >= 0 {
            return Ok(Some(n as usize));
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return Ok(None),
            _ => return Err(e),
        }
    }
}

pub struct Epoll {
    fd: RawFd,
}

pub const IN: u32 = libc::EPOLLIN as u32;
pub const OUT: u32 = libc::EPOLLOUT as u32;
pub const HUP: u32 = (libc::EPOLLHUP | libc::EPOLLERR) as u32;

impl Epoll {
    pub fn new() -> io::Result<Epoll> {
        Ok(Epoll {
            fd: cvt(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?,
        })
    }

    fn ctl(&self, op: libc::c_int, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
        let mut ev = libc::epoll_event { events, u64: token };
        cvt(unsafe { libc::epoll_ctl(self.fd, op, fd, &mut ev) })?;
        Ok(())
    }

    pub fn add(&self, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_ADD, fd, events, token)
    }

    pub fn modify(&self, fd: RawFd, events: u32, token: u64) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_MOD, fd, events, token)
    }

    pub fn del(&self, fd: RawFd) {
        let _ = self.ctl(libc::EPOLL_CTL_DEL, fd, 0, 0);
    }

    /// Wait for events. A negative timeout waits forever.
    pub fn wait(&self, timeout_ms: i32) -> Vec<(u64, u32)> {
        let mut evs = [libc::epoll_event { events: 0, u64: 0 }; 32];
        let n =
            unsafe { libc::epoll_wait(self.fd, evs.as_mut_ptr(), evs.len() as i32, timeout_ms) };
        if n <= 0 {
            return Vec::new();
        }
        evs[..n as usize]
            .iter()
            .map(|e| (e.u64, e.events))
            .collect()
    }
}

/// Block SIGTERM, SIGINT and SIGHUP and return a signalfd that reports them.
/// SIGPIPE is ignored so a vanished client is an error return, not a death.
pub fn signal_fd() -> io::Result<RawFd> {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGHUP);
        cvt(libc::sigprocmask(
            libc::SIG_BLOCK,
            &set,
            std::ptr::null_mut(),
        ))?;
        cvt(libc::signalfd(
            -1,
            &set,
            libc::SFD_NONBLOCK | libc::SFD_CLOEXEC,
        ))
    }
}

/// Read one pending signal number from a signalfd.
pub fn read_signal(fd: RawFd) -> Option<i32> {
    let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
    let n = unsafe {
        libc::read(
            fd,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<libc::signalfd_siginfo>(),
        )
    };
    if n as usize == std::mem::size_of::<libc::signalfd_siginfo>() {
        Some(info.ssi_signo as i32)
    } else {
        None
    }
}

pub struct Pty {
    pub master: RawFd,
    pub slave_path: CString,
}

pub fn set_winsize(fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    cvt(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) })?;
    Ok(())
}

pub fn open_pty(cols: u16, rows: u16) -> io::Result<Pty> {
    unsafe {
        let master = cvt(libc::posix_openpt(
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        ))?;
        let fail = |e: io::Error| {
            libc::close(master);
            e
        };
        cvt(libc::grantpt(master)).map_err(fail)?;
        cvt(libc::unlockpt(master)).map_err(fail)?;
        let mut name = [0 as libc::c_char; 128];
        if libc::ptsname_r(master, name.as_mut_ptr(), name.len()) != 0 {
            return Err(fail(io::Error::last_os_error()));
        }
        set_winsize(master, cols, rows).map_err(fail)?;
        set_nonblocking(master).map_err(fail)?;
        Ok(Pty {
            master,
            slave_path: CStr::from_ptr(name.as_ptr()).to_owned(),
        })
    }
}

pub struct Child {
    pub pid: libc::pid_t,
    pub pidfd: RawFd,
}

/// Start the program with the pty slave as its controlling terminal and its
/// stdin, stdout and stderr. An exec failure is reported with its errno.
pub fn spawn(
    program: &CString,
    argv: &[CString],
    envp: &[CString],
    slave: &CString,
) -> io::Result<Child> {
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|a| a.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());

    let mut pipe = [0 as RawFd; 2];
    cvt(unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) })?;
    let (err_r, err_w) = (pipe[0], pipe[1]);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let e = io::Error::last_os_error();
        close(err_r);
        close(err_w);
        return Err(e);
    }
    if pid == 0 {
        // The child. Only async-signal-safe calls from here to execve.
        unsafe {
            let fail = |fd: RawFd| -> ! {
                let errno = *libc::__errno_location();
                libc::write(fd, &errno as *const i32 as *const libc::c_void, 4);
                libc::_exit(127);
            };
            if libc::setsid() < 0 {
                fail(err_w);
            }
            let slave_fd = libc::open(slave.as_ptr(), libc::O_RDWR);
            if slave_fd < 0 {
                fail(err_w);
            }
            libc::ioctl(slave_fd, libc::TIOCSCTTY, 0);
            // Keep the error pipe clear of fds 0 to 2 before they are replaced.
            let err_fd = libc::fcntl(err_w, libc::F_DUPFD_CLOEXEC, 10);
            if err_fd < 0 {
                fail(err_w);
            }
            for fd in 0..3 {
                if libc::dup2(slave_fd, fd) < 0 {
                    fail(err_fd);
                }
            }
            // Close everything but stdio and the error pipe.
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                (err_fd - 1) as libc::c_uint,
                0 as libc::c_uint,
            );
            libc::syscall(
                libc::SYS_close_range,
                (err_fd + 1) as libc::c_uint,
                u32::MAX as libc::c_uint,
                0 as libc::c_uint,
            );
            // The host blocks some signals and ignores SIGPIPE; undo both.
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            for sig in [
                libc::SIGPIPE,
                libc::SIGINT,
                libc::SIGTERM,
                libc::SIGHUP,
                libc::SIGQUIT,
                libc::SIGCHLD,
            ] {
                libc::signal(sig, libc::SIG_DFL);
            }
            libc::execve(program.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
            fail(err_fd);
        }
    }

    close(err_w);
    let mut errno_buf = [0u8; 4];
    let mut got = 0usize;
    loop {
        let n = unsafe {
            libc::read(
                err_r,
                errno_buf[got..].as_mut_ptr() as *mut libc::c_void,
                4 - got,
            )
        };
        if n > 0 {
            got += n as usize;
            if got == 4 {
                break;
            }
        } else if n == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break;
        }
    }
    close(err_r);
    if got == 4 {
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        return Err(io::Error::from_raw_os_error(i32::from_ne_bytes(errno_buf)));
    }
    let pidfd = cvt(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int })?;
    Ok(Child { pid, pidfd })
}

/// How a child ended.
#[derive(Clone, Copy, Debug)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub core_dumped: bool,
}

/// Collect the exit status of a child that has exited.
pub fn reap(pid: libc::pid_t) -> Option<ExitStatus> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG,
        )
    };
    if r != 0 || unsafe { info.si_pid() } == 0 {
        return None;
    }
    let status = unsafe { info.si_status() };
    Some(match info.si_code {
        libc::CLD_EXITED => ExitStatus {
            code: Some(status),
            signal: None,
            core_dumped: false,
        },
        libc::CLD_DUMPED => ExitStatus {
            code: None,
            signal: Some(status),
            core_dumped: true,
        },
        _ => ExitStatus {
            code: None,
            signal: Some(status),
            core_dumped: false,
        },
    })
}

const SIGNALS: &[(&str, i32)] = &[
    ("HUP", libc::SIGHUP),
    ("INT", libc::SIGINT),
    ("QUIT", libc::SIGQUIT),
    ("ILL", libc::SIGILL),
    ("ABRT", libc::SIGABRT),
    ("FPE", libc::SIGFPE),
    ("KILL", libc::SIGKILL),
    ("SEGV", libc::SIGSEGV),
    ("PIPE", libc::SIGPIPE),
    ("ALRM", libc::SIGALRM),
    ("TERM", libc::SIGTERM),
    ("USR1", libc::SIGUSR1),
    ("USR2", libc::SIGUSR2),
    ("CONT", libc::SIGCONT),
    ("STOP", libc::SIGSTOP),
    ("TSTP", libc::SIGTSTP),
    ("BUS", libc::SIGBUS),
    ("TRAP", libc::SIGTRAP),
    ("WINCH", libc::SIGWINCH),
];

pub fn signal_name(sig: i32) -> String {
    match SIGNALS.iter().find(|(_, n)| *n == sig) {
        Some((name, _)) => format!("SIG{name}"),
        None => format!("SIG{sig}"),
    }
}

pub fn signal_number(name: &str) -> Option<i32> {
    let upper = name.to_ascii_uppercase();
    let short = upper.strip_prefix("SIG").unwrap_or(&upper);
    SIGNALS.iter().find(|(n, _)| *n == short).map(|(_, n)| *n)
}

/// The process group in the foreground of the terminal.
pub fn foreground_pgrp(master: RawFd) -> Option<libc::pid_t> {
    let pg = unsafe { libc::tcgetpgrp(master) };
    if pg > 0 {
        Some(pg)
    } else {
        None
    }
}

/// How many bytes of typed input the program has not read yet, or `None`
/// when that cannot be learned. The pty master does not say (its
/// `TIOCOUTQ` is always 0), so this opens the slave for a moment and asks
/// it. A terminal in canonical mode counts whole lines only: text with no
/// newline yet reads as 0.
pub fn unread_input(slave_path: &CStr) -> Option<usize> {
    unsafe {
        let fd = libc::open(
            slave_path.as_ptr(),
            libc::O_RDONLY | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        );
        if fd < 0 {
            return None;
        }
        let mut n: libc::c_int = 0;
        let r = libc::ioctl(fd, libc::FIONREAD, &mut n);
        libc::close(fd);
        if r < 0 || n < 0 {
            None
        } else {
            Some(n as usize)
        }
    }
}
/// Whether any process is left in a process group.
pub fn group_exists(pg: libc::pid_t) -> bool {
    let r = unsafe { libc::kill(-pg, 0) };
    r == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The short name of a process, from /proc.
pub fn process_name(pid: libc::pid_t) -> Option<String> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let name = s.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// The uid of the process at the other end of a unix socket.
pub fn peer_uid(fd: RawFd) -> Option<libc::uid_t> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if r == 0 {
        Some(cred.uid)
    } else {
        None
    }
}
