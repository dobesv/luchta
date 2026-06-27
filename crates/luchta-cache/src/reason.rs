use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum RunReason {
    NoPriorRecord,
    PriorFailed,
    NonceChanged,
    TaskSpecMismatch,
    DepOutputMismatch {
        tasks: Vec<String>,
    },
    PkgDepMismatch,
    EnvMismatch,
    InputChanged {
        changed: Vec<FileDelta>,
        truncated: bool,
        change_count: u32,
    },
    OutputChanged {
        changed: Vec<FileDelta>,
        truncated: bool,
        change_count: u32,
    },
    CacheHit,
    SharedCacheHit,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FileDelta {
    pub path: String,
    pub prior_hash: [u8; 32],
    pub current_hash: [u8; 32],
    pub prior_absent: bool,
    pub current_absent: bool,
}

impl RunReason {
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::NoPriorRecord => "no prior run".to_string(),
            Self::PriorFailed => "previous run failed".to_string(),
            Self::NonceChanged => "cache nonce changed".to_string(),
            Self::TaskSpecMismatch => "task definition changed".to_string(),
            Self::DepOutputMismatch { tasks } => {
                if tasks.is_empty() {
                    "dependency output changed".to_string()
                } else {
                    format!("dependency output changed: {}", tasks.join(", "))
                }
            }
            Self::PkgDepMismatch => "package dependencies changed".to_string(),
            Self::EnvMismatch => "env changed".to_string(),
            Self::InputChanged { change_count, .. } => {
                if *change_count > 0 {
                    format!("input changed ({change_count} changes)")
                } else {
                    "input changed".to_string()
                }
            }
            Self::OutputChanged { change_count, .. } => {
                if *change_count > 0 {
                    format!("output changed ({change_count} changes)")
                } else {
                    "output changed".to_string()
                }
            }
            Self::CacheHit => "up to date (local cache hit)".to_string(),
            Self::SharedCacheHit => "restored from shared cache".to_string(),
        }
    }
}

impl fmt::Display for RunReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::{FileDelta, RunReason};
    use bincode::config;

    #[test]
    fn run_reason_bincode_round_trip_all_variants() {
        let reasons = vec![
            RunReason::NoPriorRecord,
            RunReason::PriorFailed,
            RunReason::NonceChanged,
            RunReason::TaskSpecMismatch,
            RunReason::DepOutputMismatch {
                tasks: vec!["@pkg#build".to_string(), "@util#test".to_string()],
            },
            RunReason::DepOutputMismatch { tasks: Vec::new() },
            RunReason::PkgDepMismatch,
            RunReason::EnvMismatch,
            RunReason::InputChanged {
                changed: vec![
                    FileDelta {
                        path: "src/main.rs".to_string(),
                        prior_hash: [1; 32],
                        current_hash: [2; 32],
                        prior_absent: false,
                        current_absent: false,
                    },
                    FileDelta {
                        path: "src/new.rs".to_string(),
                        prior_hash: [0; 32],
                        current_hash: [3; 32],
                        prior_absent: true,
                        current_absent: false,
                    },
                ],
                truncated: true,
                change_count: 7,
            },
            RunReason::OutputChanged {
                changed: vec![
                    FileDelta {
                        path: "dist/app.js".to_string(),
                        prior_hash: [4; 32],
                        current_hash: [5; 32],
                        prior_absent: false,
                        current_absent: false,
                    },
                    FileDelta {
                        path: "dist/extra.js".to_string(),
                        prior_hash: [0; 32],
                        current_hash: [6; 32],
                        prior_absent: true,
                        current_absent: false,
                    },
                ],
                truncated: true,
                change_count: 7,
            },
            RunReason::CacheHit,
            RunReason::SharedCacheHit,
        ];

        for reason in reasons {
            let config = config::standard()
                .with_fixed_int_encoding()
                .with_little_endian();
            let encoded = bincode::serde::encode_to_vec(&reason, config).unwrap();
            let (decoded, consumed): (RunReason, usize) =
                bincode::serde::decode_from_slice(&encoded, config).unwrap();
            assert_eq!(consumed, encoded.len());
            assert_eq!(decoded, reason);
            assert!(!reason.summary().is_empty());
            assert!(!reason.to_string().is_empty());
        }
    }
}
