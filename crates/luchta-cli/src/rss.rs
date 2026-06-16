pub fn process_tree_rss_bytes() -> Option<u64> {
    process_tree_rss_bytes_for(std::process::id())
}

pub fn process_tree_rss_bytes_for(root_pid: u32) -> Option<u64> {
    platform::process_tree_rss_bytes_for(root_pid)
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

    pub(super) fn process_tree_rss_bytes_for(root_pid: u32) -> Option<u64> {
        let mut total = process_rss_bytes(root_pid)?;
        let mut visited = HashSet::from([root_pid]);
        let mut queue = VecDeque::from([root_pid]);

        while let Some(pid) = queue.pop_front() {
            for child_pid in process_children(pid) {
                if !visited.insert(child_pid) {
                    continue;
                }

                let Some(child_rss) = process_rss_bytes(child_pid) else {
                    continue;
                };

                total = total.saturating_add(child_rss);
                queue.push_back(child_pid);
            }
        }

        Some(total)
    }

    fn process_rss_bytes(pid: u32) -> Option<u64> {
        let status = fs::read_to_string(status_path(pid)).ok()?;
        parse_status_rss_bytes(&status)
    }

    fn process_children(pid: u32) -> Vec<u32> {
        let mut children = HashSet::new();
        let Ok(task_entries) = fs::read_dir(task_dir_path(pid)) else {
            return Vec::new();
        };

        for task_entry in task_entries.flatten() {
            let Some(tid) = task_entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };

            let Ok(task_children) = fs::read_to_string(task_children_path(pid, &tid)) else {
                continue;
            };

            children.extend(parse_children(&task_children));
        }

        children.into_iter().collect()
    }

    fn status_path(pid: u32) -> PathBuf {
        PathBuf::from("/proc").join(pid.to_string()).join("status")
    }

    fn task_dir_path(pid: u32) -> PathBuf {
        PathBuf::from("/proc").join(pid.to_string()).join("task")
    }

    fn task_children_path(pid: u32, tid: &str) -> PathBuf {
        task_dir_path(pid).join(tid).join("children")
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

    /// Parse the space-separated PIDs from a `/proc/<pid>/task/<tid>/children`
    /// file. Best-effort: skip any token that does not parse as a PID rather
    /// than discarding the whole (kernel-generated) list, so one odd token
    /// can't drop an entire thread's children from the RSS walk.
    fn parse_children(children: &str) -> Vec<u32> {
        children
            .split_whitespace()
            .filter_map(|pid| pid.parse::<u32>().ok())
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use std::io;
        use std::process::{Child, Command};
        use std::thread;
        use std::time::Duration;

        use std::collections::{HashSet, VecDeque};

        use super::{
            parse_children, parse_status_rss_bytes, process_children, process_rss_bytes,
            process_tree_rss_bytes_for,
        };

        /// Walk the descendant tree of `root_pid` (same discovery as
        /// `process_tree_rss_bytes_for`) and report whether `needle` is in it.
        fn process_tree_contains(root_pid: u32, needle: u32) -> bool {
            let mut visited = HashSet::from([root_pid]);
            let mut queue = VecDeque::from([root_pid]);
            while let Some(pid) = queue.pop_front() {
                if pid == needle {
                    return true;
                }
                for child_pid in process_children(pid) {
                    if visited.insert(child_pid) {
                        queue.push_back(child_pid);
                    }
                }
            }
            visited.contains(&needle)
        }

        fn wait_for_process_absence(pid: u32, attempts: usize, delay: Duration) {
            for _ in 0..attempts {
                if process_rss_bytes(pid).is_none() {
                    return;
                }
                thread::sleep(delay);
            }
        }

        #[test]
        fn process_tree_rss_is_available_for_current_process() {
            let rss = process_tree_rss_bytes_for(std::process::id());
            assert!(rss.is_some_and(|bytes| bytes > 0));
        }

        #[test]
        fn invalid_root_pid_returns_none() {
            assert_eq!(process_tree_rss_bytes_for(u32::MAX), None);
        }

        #[test]
        fn process_tree_rss_includes_child_spawned_from_non_main_thread() {
            let root_pid = std::process::id();
            let self_rss = process_rss_bytes(root_pid).expect("current process rss should exist");
            let mut child =
                spawn_sleep_from_non_main_thread().expect("spawn child from worker thread");
            wait_for_process_presence(child.id(), 50, Duration::from_millis(100))
                .expect("child should appear in /proc");

            let tree_rss = process_tree_rss_bytes_for(root_pid).expect("tree rss should exist");
            let child_rss = process_rss_bytes(child.id()).expect("child rss should exist");

            // Core regression guard: a child spawned from a NON-main thread must be
            // discovered. The old main-TID-only walk would miss it, so tree RSS would
            // not reflect the child at all. With the all-threads walk it is included.
            assert!(tree_rss > self_rss);
            assert!(tree_rss >= self_rss.saturating_add(child_rss));
            assert!(
                process_tree_contains(root_pid, child.id()),
                "child pid {} spawned from a non-main thread must appear in the tree walk",
                child.id()
            );

            terminate_child(&mut child);

            // Statelessness guard: each call re-reads /proc, so once the child is gone
            // and reaped it must no longer be counted. We avoid asserting on the total
            // byte delta (the parent's own RSS can move between samples, making a strict
            // `tree_after <= tree_rss - child_rss` bound flaky); instead assert the dead
            // child pid is no longer part of the descendant set.
            wait_for_process_absence(child.id(), 50, Duration::from_millis(100));
            assert!(
                !process_tree_contains(root_pid, child.id()),
                "reaped child pid {} must not be counted after exit",
                child.id()
            );
        }

        #[test]
        fn malformed_status_returns_none() {
            let status = "Name:\tluchta\nVmRSS:\tnot-a-number kB\n";
            assert_eq!(parse_status_rss_bytes(status), None);
        }

        #[test]
        fn malformed_children_skips_bad_tokens() {
            // A single unparseable token must not discard the whole list; the
            // valid PIDs are still returned so the walk doesn't lose children.
            assert_eq!(parse_children("123 abc 456"), vec![123, 456]);
            assert_eq!(parse_children(""), Vec::<u32>::new());
        }

        fn spawn_sleep_from_non_main_thread() -> io::Result<Child> {
            thread::spawn(|| Command::new("sleep").arg("30").spawn())
                .join()
                .expect("worker thread should not panic")
        }

        fn wait_for_process_presence(pid: u32, attempts: usize, delay: Duration) -> Option<u64> {
            for _ in 0..attempts {
                if let Some(rss) = process_rss_bytes(pid) {
                    return Some(rss);
                }
                thread::sleep(delay);
            }
            None
        }

        fn terminate_child(child: &mut Child) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    #[allow(dead_code)] // Stub compiled on non-Linux; Linux /proc implementation is unavailable there.
    pub(super) fn process_tree_rss_bytes_for(_root_pid: u32) -> Option<u64> {
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
