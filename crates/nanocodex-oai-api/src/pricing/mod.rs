//! Built-in USD estimates for the supported `OpenAI` model.
//!
//! Nanocodex supports only [`crate::MODEL`], so callers do not configure a
//! pricing catalog. Completed responses are priced automatically from the
//! provider-reported token usage and the standard or priority service tier
//! selected by the request.
//!
//! Rates are sourced from the
//! [OpenAI API pricing page](https://developers.openai.com/api/docs/pricing):
//!
//! | Service tier | Input | Cached input | Cache write | Output |
//! | --- | ---: | ---: | ---: | ---: |
//! | Standard | $5.00 | $0.50 | $6.25 | $30.00 |
//! | Priority | $10.00 | $1.00 | $12.50 | $60.00 |
//!
//! Prices are per one million tokens. Reasoning tokens are already included
//! in output tokens and are not charged a second time.

mod amount;
mod estimate;

use serde::{Deserialize, Serialize};

pub use amount::UsdAmount;
pub use estimate::{EstimatedUsdCost, ServiceTier, estimate};

/// Stable revision for the built-in model and service-tier pricing catalog.
pub const PRICING_REVISION: &str = "gpt-5.6-sol-standard-priority-v1";

/// Availability of the automatic local USD estimate.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CostStatus {
    /// Provider usage was priced using the built-in rates.
    EstimatedFromUsage,
    /// Observed usage was priced, but the estimate is only a lower bound.
    ///
    /// One or more provider responses omitted usage, or accepted work may
    /// have consumed unreported tokens.
    EstimatedLowerBound,
    /// The provider omitted usage from the completed response.
    #[default]
    UsageNotReported,
    /// No billable provider response was created for this operation.
    NotApplicable,
    /// A retained record used a newer or unknown status.
    #[serde(other)]
    Other,
}

impl CostStatus {
    /// Returns the stable snake-case wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EstimatedFromUsage => "estimated_from_usage",
            Self::EstimatedLowerBound => "estimated_lower_bound",
            Self::UsageNotReported => "usage_not_reported",
            Self::NotApplicable => "not_applicable",
            Self::Other => "other",
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::CostStatus;

    #[test]
    fn cost_status_has_a_forward_compatible_wire_shape() {
        assert_eq!(
            serde_json::to_value(CostStatus::EstimatedFromUsage).unwrap(),
            json!("estimated_from_usage")
        );
        assert_eq!(
            serde_json::to_value(CostStatus::EstimatedLowerBound).unwrap(),
            json!("estimated_lower_bound")
        );
        assert_eq!(
            serde_json::to_value(CostStatus::NotApplicable).unwrap(),
            json!("not_applicable")
        );
        assert_eq!(
            serde_json::from_value::<CostStatus>(json!("future_status")).unwrap(),
            CostStatus::Other
        );
    }
}
