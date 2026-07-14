/// Prints `<bin_name> <version>` and returns true when `args` contains `--version` or `-V`.
pub fn version_requested(args: &[String], bin_name: &str, version: &str) -> bool {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--version" | "-V"))
    {
        println!("{bin_name} {version}");
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::version_requested;

    #[test]
    fn returns_true_and_matches_long_flag() {
        assert!(version_requested(
            &["prog".to_owned(), "--version".to_owned()],
            "prog",
            "1.2.3"
        ));
    }

    #[test]
    fn returns_true_and_matches_short_flag() {
        assert!(version_requested(
            &["prog".to_owned(), "-V".to_owned()],
            "prog",
            "1.2.3"
        ));
    }

    #[test]
    fn returns_false_when_flag_absent() {
        assert!(!version_requested(
            &["prog".to_owned(), "--help".to_owned()],
            "prog",
            "1.2.3"
        ));
    }
}
