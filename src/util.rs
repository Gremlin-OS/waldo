//! Small things written by hand to avoid a dependency: timestamps and ids.

use std::fs::File;
use std::io::Read;

/// The current wall-clock time as RFC 3339 UTC with milliseconds.
pub fn rfc3339_now() -> String {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    rfc3339(ts.tv_sec, (ts.tv_nsec / 1_000_000) as u32)
}

pub fn rfc3339(secs: i64, millis: u32) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        millis
    )
}

/// Seconds since the epoch.
pub fn unix_now() -> i64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    ts.tv_sec
}

fn random_bytes(buf: &mut [u8]) {
    let n = unsafe { libc::getrandom(buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
    if n as usize == buf.len() {
        return;
    }
    if let Ok(mut f) = File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    // Last resort; ids only need to be unlikely to collide.
    let seed = unix_now() as u64 ^ (std::process::id() as u64) << 32;
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (seed >> ((i % 8) * 8)) as u8;
    }
}

/// `len` lowercase base32 characters.
pub fn random_id(len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut raw = vec![0u8; len];
    random_bytes(&mut raw);
    raw.iter().map(|b| ALPHABET[(b & 31) as usize] as char).collect()
}

/// A duration for people: `0.2s`, `54.6s`.
pub fn seconds(ms: u64) -> String {
    format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_795_045_267, 120), "2026-11-18T23:41:07.120Z");
        assert_eq!(rfc3339(951_782_400, 5), "2000-02-29T00:00:00.005Z");
    }

    #[test]
    fn ids() {
        let id = random_id(6);
        assert_eq!(id.len(), 6);
        assert!(id.chars().all(|c| c.is_ascii_lowercase() || ('2'..='7').contains(&c)));
        assert_ne!(random_id(12), random_id(12));
    }
}
