//! Host facts. Only `hostname` is ported so far (the rest of `sysinfo.py`
//! feeds the LCD pages).

/// `socket.gethostname()`.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for buf.len() bytes; gethostname NUL-terminates on success.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn hostname_matches_proc() {
        let proc = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        assert_eq!(super::hostname(), proc.trim());
    }
}
