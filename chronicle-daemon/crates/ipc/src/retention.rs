//! The configured retention policy, parsed once.
//!
//! Lives here rather than in `chronicle-storage` so that the status wire type
//! and the cleanup path can share one definition — see ADR-015. Names no
//! database type, which is what keeps a move to a shared crate a one-file
//! relocation.

use serde::{Deserialize, Serialize};

/// Largest accepted retention window: 100 years. Past this a value is a config
/// error, not a policy — unchecked, a large enough one would wrap
/// `compute_cutoff`'s arithmetic into the future and expire the whole database.
/// `0` already means "keep forever", so it is not the intuitive spelling for
/// that.
pub const MAX_RETENTION_DAYS: i64 = 36_500;

/// Retention when the config row is absent.
pub const DEFAULT_RETENTION_DAYS: u32 = 30;

/// Why a configured value was refused. Never serialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    NotANumber,
    Negative,
    AboveMax,
}

/// What the `retention_days` setting means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Retention {
    Days { value: u32 },
    Disabled,
    Invalid,
}

/// Hand-written because `#[derive(Default)]` can only select a unit variant,
/// and the default is a day count.
impl Default for Retention {
    fn default() -> Self {
        Self::Days {
            value: DEFAULT_RETENTION_DAYS,
        }
    }
}

impl Retention {
    /// The only place these rules are stated; `parse` and `from_stored`
    /// delegate here. `Days`'s field is public, so this is not the only way to
    /// build a `Retention` — a caller or `Deserialize` can still bypass it.
    ///
    /// The bound check duplicates the one in `chronicle-storage`'s
    /// `run_cleanup_interruptible`. docs/development/storage.md, "Two guards on
    /// one invariant means the outer one is untestable", argues against exactly
    /// that — but its test is whether an assertion can tell the two apart. This
    /// one is distinguishable: delete the arm and
    /// `one_past_the_bound_is_refused_as_above_max` fails.
    pub fn classify(raw: &str) -> Result<Self, Rejected> {
        let n: i64 = raw.trim().parse().map_err(|_| Rejected::NotANumber)?;
        match n {
            0 => Ok(Self::Disabled),
            n if n < 0 => Err(Rejected::Negative),
            n if n > MAX_RETENTION_DAYS => Err(Rejected::AboveMax),
            n => Ok(Self::Days { value: n as u32 }),
        }
    }

    /// The read path. A refused value is [`Retention::Invalid`]; the reason is
    /// dropped because nothing downstream of the wire reads it.
    pub fn parse(raw: &str) -> Self {
        Self::classify(raw).unwrap_or(Self::Invalid)
    }

    /// The stored-value path. An absent row is the default, not a fault.
    ///
    /// Migration 001 seeds the key and always has, so `None` means the row was
    /// removed by hand rather than a database that predates the seed.
    pub fn from_stored(raw: Option<&str>) -> Self {
        match raw {
            Some(v) => Self::parse(v),
            None => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_disabled() {
        assert_eq!(Retention::classify("0"), Ok(Retention::Disabled));
    }

    #[test]
    fn a_positive_value_within_the_bound_is_days() {
        assert_eq!(Retention::classify("30"), Ok(Retention::Days { value: 30 }));
    }

    #[test]
    fn the_bound_itself_is_accepted() {
        let at_bound = MAX_RETENTION_DAYS.to_string();
        assert_eq!(
            Retention::classify(&at_bound),
            Ok(Retention::Days {
                value: MAX_RETENTION_DAYS as u32
            })
        );
    }

    #[test]
    fn one_past_the_bound_is_refused_as_above_max() {
        let past = (MAX_RETENTION_DAYS + 1).to_string();
        assert_eq!(Retention::classify(&past), Err(Rejected::AboveMax));
    }

    #[test]
    fn a_negative_value_is_refused_as_negative() {
        assert_eq!(Retention::classify("-1"), Err(Rejected::Negative));
    }

    #[test]
    fn text_is_refused_as_not_a_number() {
        assert_eq!(Retention::classify("thirty"), Err(Rejected::NotANumber));
    }

    #[test]
    fn a_value_too_large_for_i64_is_not_a_number() {
        assert_eq!(
            Retention::classify("99999999999999999999"),
            Err(Rejected::NotANumber)
        );
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        assert_eq!(
            Retention::classify(" 30 "),
            Ok(Retention::Days { value: 30 })
        );
    }

    #[test]
    fn parse_collapses_every_refusal_to_invalid() {
        for raw in ["thirty", "-1", "99999999999999999999"] {
            assert_eq!(Retention::parse(raw), Retention::Invalid, "raw={raw}");
        }
        let past = (MAX_RETENTION_DAYS + 1).to_string();
        assert_eq!(Retention::parse(&past), Retention::Invalid);
    }

    #[test]
    fn an_absent_row_is_the_default_not_a_refusal() {
        assert_eq!(
            Retention::from_stored(None),
            Retention::Days {
                value: DEFAULT_RETENTION_DAYS
            }
        );
    }

    #[test]
    fn a_present_row_goes_through_parse() {
        assert_eq!(Retention::from_stored(Some("0")), Retention::Disabled);
        assert_eq!(Retention::from_stored(Some("bad")), Retention::Invalid);
    }

    #[test]
    fn the_default_is_the_documented_retention() {
        assert_eq!(
            Retention::default(),
            Retention::Days {
                value: DEFAULT_RETENTION_DAYS
            }
        );
    }

    #[test]
    fn the_policy_numbers_are_what_the_design_says() {
        // The literals themselves. Assertions elsewhere are written against
        // the constants, so several of them hold whatever the constants say.
        // BR-9 names 30; AD-5 names 36,500.
        assert_eq!(DEFAULT_RETENTION_DAYS, 30);
        assert_eq!(MAX_RETENTION_DAYS, 36_500);
    }

    #[test]
    fn the_default_is_a_value_classify_accepts() {
        // Guards the constants against each other. A DEFAULT_RETENTION_DAYS
        // of 0 would make `from_stored(None)` return `Days { value: 0 }` where
        // `classify("0")` says `Disabled`; one above the bound would return a
        // value `classify` refuses.
        assert_eq!(
            Retention::classify(&DEFAULT_RETENTION_DAYS.to_string()),
            Ok(Retention::default())
        );
    }

    #[test]
    fn an_empty_value_is_not_a_number() {
        assert_eq!(Retention::classify(""), Err(Rejected::NotANumber));
        assert_eq!(Retention::classify("   "), Err(Rejected::NotANumber));
    }

    #[test]
    fn a_leading_plus_is_accepted() {
        // Rust's i64 parser takes it. Recorded rather than guarded — a
        // hand-edited row spelling "+30" means 30 days.
        assert_eq!(
            Retention::classify("+30"),
            Ok(Retention::Days { value: 30 })
        );
    }

    #[test]
    fn each_variant_serializes_to_its_documented_shape() {
        let days = serde_json::to_string(&Retention::Days { value: 30 }).unwrap();
        assert_eq!(days, r#"{"kind":"days","value":30}"#);
        assert_eq!(
            serde_json::to_string(&Retention::Disabled).unwrap(),
            r#"{"kind":"disabled"}"#
        );
        assert_eq!(
            serde_json::to_string(&Retention::Invalid).unwrap(),
            r#"{"kind":"invalid"}"#
        );
    }

    #[test]
    fn every_variant_round_trips() {
        for v in [
            Retention::Days { value: 30 },
            Retention::Disabled,
            Retention::Invalid,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(serde_json::from_str::<Retention>(&json).unwrap(), v);
        }
    }
}
