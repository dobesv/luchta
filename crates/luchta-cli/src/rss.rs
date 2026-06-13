pub fn process_tree_rss_bytes() -> Option<u64> {
    platform::process_tree_rss_bytes()
}

pub fn format_rss(bytes: Option<u64>) -> String {
    match bytes {
        Some(bytes) if bytes >= GIB => format_with_unit(bytes, GIB, "GB"),
        Some(bytes) if bytes >= MIB => format_with_unit(bytes, MIB, "MB"),
        Some(bytes) if bytes >= KIB => format_with_unit(bytes, KIB, "KB"),
        Some(bytes) => format!("{bytes} B"),
        None => "unavailable".to_owned(),
    }
}

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

fn format_with_unit(bytes: u64, unit_size: u64, unit_label: &str) -> String {
    let whole = bytes / unit_size;
    let remainder = bytes % unit_size;

    if remainder == 0 {
        return format!("{whole} {unit_label}");
    }

    let tenths = (remainder.saturating_mul(10) + (unit_size / 2)) / unit_size;
    if tenths == 10 {
        format!("{} {unit_label}", whole + 1)
    } else {
        format!("{whole}.{tenths} {unit_label}")
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::collections::{HashSet, VecDeque};
    use std::fs;
    use std::path::PathBuf;

    pub(super) fn process_tree_rss_bytes() -> Option<u64> {
        let root_pid = std::process::id();
        let mut visited = HashSet::from([root_pid]);
        let mut queue = VecDeque::from([root_pid]);
        let mut total = read_status_rss_bytes(status_path(root_pid))?;

        while let Some(pid) = queue.pop_front() {
            // Best-effort: a child can exit between enumeration and read, leaving
            // its children file unreadable. Skip such entries rather than failing
            // the whole tree walk.
            let Some(children) = read_children(task_children_path(pid)) else {
                continue;
            };
            for child_pid in children {
                if !visited.insert(child_pid) {
                    continue;
                }

                if let Some(child_rss) = read_status_rss_bytes(status_path(child_pid)) {
                    total = total.saturating_add(child_rss);
                    queue.push_back(child_pid);
                }
            }
        }

        Some(total)
    }

    fn status_path(pid: u32) -> PathBuf {
        PathBuf::from("/proc").join(pid.to_string()).join("status")
    }

    fn task_children_path(pid: u32) -> PathBuf {
        PathBuf::from("/proc")
            .join(pid.to_string())
            .join("task")
            .join(pid.to_string())
            .join("children")
    }

    fn read_status_rss_bytes(path: PathBuf) -> Option<u64> {
        let status = fs::read_to_string(path).ok()?;
        parse_status_rss_bytes(&status)
    }

    fn read_children(path: PathBuf) -> Option<Vec<u32>> {
        let children = fs::read_to_string(path).ok()?;
        parse_children(&children)
    }

    fn parse_status_rss_bytes(status: &str) -> Option<u64> {
        status.lines().find_map(parse_vmrss_line)
    }

    fn parse_vmrss_line(line: &str) -> Option<u64> {
        let value = line.strip_prefix("VmRSS:")?;
        parse_kib_value(value)
    }

    fn parse_kib_value(value: &str) -> Option<u64> {
        let mut parts = value.split_whitespace();
        let amount_kib = parts.next()?.parse::<u64>().ok()?;
        match parts.next() {
            Some("kB") if parts.next().is_none() => amount_kib.checked_mul(1024),
            _ => None,
        }
    }

    fn parse_children(children: &str) -> Option<Vec<u32>> {
        children
            .split_whitespace()
            .map(|pid| pid.parse::<u32>().ok())
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::{parse_children, parse_status_rss_bytes};

        #[test]
        fn process_tree_rss_is_available_for_current_process() {
            let rss = super::process_tree_rss_bytes();
            assert!(rss.is_some_and(|bytes| bytes > 0));
        }

        #[test]
        fn malformed_status_returns_none() {
            let status = "Name:\tluchta\nVmRSS:\tnot-a-number kB\n";
            assert_eq!(parse_status_rss_bytes(status), None);
        }

        #[test]
        fn malformed_children_returns_none() {
            assert_eq!(parse_children("123 abc 456"), None);
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    #[allow(dead_code)] // Stub compiled on non-Linux; Linux /proc implementation is unavailable there.
    pub(super) fn process_tree_rss_bytes() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{format_rss, GIB, KIB, MIB};

    #[test]
    fn format_rss_none_is_unavailable() {
        assert_eq!(format_rss(None), "unavailable");
    }

    #[test]
    fn format_rss_uses_expected_units() {
        assert_eq!(format_rss(Some(900 * KIB)), "900 KB");
        assert_eq!(format_rss(Some(MIB + (MIB / 2))), "1.5 MB");
        assert_eq!(format_rss(Some(2 * GIB)), "2 GB");
    }
}
