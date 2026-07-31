//! Pure quota and spend-cap admission decisions.
//!
//! The edge or control plane owns the authoritative counters. This module only
//! performs the overflow-safe decision so every deployment applies identical
//! boundary semantics.

/// Tenant limits. `None` means unbounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Limits {
    /// Maximum metered units in the control plane's current window.
    pub max_units: Option<u64>,
    /// Maximum spend in millionths of the configured currency unit.
    pub max_spend_micros: Option<u64>,
}

/// Usage already committed in the current window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Metered units already committed.
    pub units: u64,
    /// Spend already committed, in millionths of a currency unit.
    pub spend_micros: u64,
}

/// Successful reservation or the reason admission was denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitDecision {
    /// Admission succeeds with these new committed totals.
    Allowed(Usage),
    /// Admission would violate a configured limit.
    Rejected(LimitReason),
}

impl LimitDecision {
    /// Whether the reservation may proceed.
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed(_))
    }
}

/// Why a usage reservation was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitReason {
    /// The unit quota would be exceeded.
    QuotaExceeded,
    /// The monetary spend cap would be exceeded.
    SpendCapExceeded,
    /// Calculating the requested total exceeded the representable range.
    ArithmeticOverflow,
}

/// Atomically assess a proposed usage increment against quota and spend caps.
///
/// `unit_price_micros` is the monetary price of one metered unit. Exact boundary
/// values are allowed: a limit rejects only when the new total is greater.
#[must_use]
pub fn assess_limits(
    current: Usage,
    requested_units: u64,
    unit_price_micros: u64,
    limits: Limits,
) -> LimitDecision {
    let Some(units) = current.units.checked_add(requested_units) else {
        return LimitDecision::Rejected(LimitReason::ArithmeticOverflow);
    };
    if limits.max_units.is_some_and(|limit| units > limit) {
        return LimitDecision::Rejected(LimitReason::QuotaExceeded);
    }

    let Some(incremental_spend) = requested_units.checked_mul(unit_price_micros) else {
        return LimitDecision::Rejected(LimitReason::ArithmeticOverflow);
    };
    let Some(spend_micros) = current.spend_micros.checked_add(incremental_spend) else {
        return LimitDecision::Rejected(LimitReason::ArithmeticOverflow);
    };
    if limits
        .max_spend_micros
        .is_some_and(|limit| spend_micros > limit)
    {
        return LimitDecision::Rejected(LimitReason::SpendCapExceeded);
    }

    LimitDecision::Allowed(Usage {
        units,
        spend_micros,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_boundaries_are_allowed() {
        let decision = assess_limits(
            Usage {
                units: 8,
                spend_micros: 800,
            },
            2,
            100,
            Limits {
                max_units: Some(10),
                max_spend_micros: Some(1_000),
            },
        );
        assert_eq!(
            decision,
            LimitDecision::Allowed(Usage {
                units: 10,
                spend_micros: 1_000
            })
        );
    }

    #[test]
    fn quota_is_checked_before_spend() {
        let decision = assess_limits(
            Usage::default(),
            11,
            1_000,
            Limits {
                max_units: Some(10),
                max_spend_micros: Some(1),
            },
        );
        assert_eq!(
            decision,
            LimitDecision::Rejected(LimitReason::QuotaExceeded)
        );
    }

    #[test]
    fn spend_cap_rejects_the_first_unit_over() {
        let decision = assess_limits(
            Usage {
                units: 2,
                spend_micros: 200,
            },
            1,
            101,
            Limits {
                max_units: None,
                max_spend_micros: Some(300),
            },
        );
        assert_eq!(
            decision,
            LimitDecision::Rejected(LimitReason::SpendCapExceeded)
        );
    }

    #[test]
    fn arithmetic_overflow_never_wraps_into_an_allowance() {
        assert_eq!(
            assess_limits(Usage::default(), u64::MAX, 2, Limits::default()),
            LimitDecision::Rejected(LimitReason::ArithmeticOverflow)
        );
        assert_eq!(
            assess_limits(
                Usage {
                    units: u64::MAX,
                    spend_micros: 0
                },
                1,
                0,
                Limits::default()
            ),
            LimitDecision::Rejected(LimitReason::ArithmeticOverflow)
        );
    }
}
