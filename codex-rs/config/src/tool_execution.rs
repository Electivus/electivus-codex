use std::time::Duration;
use std::time::Instant;

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

const DEFAULT_TIMEOUT_MIN_MS: u64 = 10_000;
const DEFAULT_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_TIMEOUT_MAX_MS: u64 = 3_600_000;
const DEFAULT_YIELD_MIN_MS: u64 = 10_000;
const DEFAULT_YIELD_MS: u64 = 30_000;
const DEFAULT_YIELD_MAX_MS: u64 = 300_000;

/// Partial user configuration for model-controlled command execution timing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolExecutionToml {
    /// Hard command deadline range. Omitted fields use 10000 / 600000 / 3600000 ms.
    pub timeout: Option<ToolExecutionTimingRangeToml>,
    /// Observation yield range. Omitted fields use 10000 / 30000 / 300000 ms.
    #[serde(rename = "yield")]
    pub yield_time: Option<ToolExecutionTimingRangeToml>,
}

/// Partial user configuration for one execution timing range. Values must be positive,
/// representable as host wait deadlines, and resolve to `min_ms <= default_ms <= max_ms`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ToolExecutionTimingRangeToml {
    /// Smallest model-requested value Codex permits.
    #[schemars(range(min = 1))]
    pub min_ms: Option<u64>,
    /// Value used when the model omits the timing argument.
    #[schemars(range(min = 1))]
    pub default_ms: Option<u64>,
    /// Largest model-requested value Codex permits.
    #[schemars(range(min = 1))]
    pub max_ms: Option<u64>,
}

/// Validated timing policy used by model-controlled execution surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ToolExecutionPolicy {
    timeout: ToolExecutionTimingRange,
    #[serde(rename = "yield")]
    yield_time: ToolExecutionTimingRange,
}

/// A positive, ordered range with the default inside its bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ToolExecutionTimingRange {
    min_ms: u64,
    default_ms: u64,
    max_ms: u64,
}

/// The result of applying a timing range to an optional model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTiming {
    pub effective_ms: u64,
    pub adjustment: Option<TimingAdjustment>,
}

/// A model timing request that was moved to an effective policy bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingAdjustment {
    pub requested_ms: u64,
    pub effective_ms: u64,
    pub bound: TimingBound,
}

/// Identifies which policy bound adjusted a model timing request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingBound {
    Minimum,
    Maximum,
}

impl Default for ToolExecutionPolicy {
    fn default() -> Self {
        Self {
            timeout: Self::default_timeout(),
            yield_time: Self::default_yield(),
        }
    }
}

impl ToolExecutionPolicy {
    pub const fn new(
        timeout: ToolExecutionTimingRange,
        yield_time: ToolExecutionTimingRange,
    ) -> Self {
        Self {
            timeout,
            yield_time,
        }
    }

    pub const fn timeout(self) -> ToolExecutionTimingRange {
        self.timeout
    }

    pub const fn yield_time(self) -> ToolExecutionTimingRange {
        self.yield_time
    }

    pub fn resolve(
        config: Option<&ToolExecutionToml>,
        legacy_yield_max_ms: Option<u64>,
    ) -> Result<Self, String> {
        let timeout = config.and_then(|config| config.timeout.as_ref());
        let yield_time = config.and_then(|config| config.yield_time.as_ref());
        if legacy_yield_max_ms.is_some() && yield_time.and_then(|range| range.max_ms).is_some() {
            return Err(
                "`background_terminal_max_timeout` conflicts with `tool_execution.yield.max_ms`; remove the deprecated key"
                    .to_string(),
            );
        }
        // The old key accepted every `u64` and normalized very small values at runtime. Preserve
        // config loading while respecting the new built-in default, which must remain inside the
        // effective range.
        let legacy_yield_max_ms = legacy_yield_max_ms.map(normalize_legacy_yield_max_ms);

        Ok(Self {
            timeout: ToolExecutionTimingRange::resolve(
                "tool_execution.timeout",
                timeout,
                Self::default_timeout(),
                /*legacy_max_ms*/ None,
            )?,
            yield_time: ToolExecutionTimingRange::resolve(
                "tool_execution.yield",
                yield_time,
                Self::default_yield(),
                legacy_yield_max_ms,
            )?,
        })
    }

    pub fn to_toml(self) -> ToolExecutionToml {
        ToolExecutionToml {
            timeout: Some(self.timeout.to_toml()),
            yield_time: Some(self.yield_time.to_toml()),
        }
    }

    const fn default_timeout() -> ToolExecutionTimingRange {
        ToolExecutionTimingRange {
            min_ms: DEFAULT_TIMEOUT_MIN_MS,
            default_ms: DEFAULT_TIMEOUT_MS,
            max_ms: DEFAULT_TIMEOUT_MAX_MS,
        }
    }

    const fn default_yield() -> ToolExecutionTimingRange {
        ToolExecutionTimingRange {
            min_ms: DEFAULT_YIELD_MIN_MS,
            default_ms: DEFAULT_YIELD_MS,
            max_ms: DEFAULT_YIELD_MAX_MS,
        }
    }
}

fn normalize_legacy_yield_max_ms(max_ms: u64) -> u64 {
    let max_ms = max_ms.max(DEFAULT_YIELD_MS);
    let now = Instant::now();
    if now.checked_add(Duration::from_millis(max_ms)).is_some() {
        return max_ms;
    }

    // Some supported hosts cannot represent the largest `u64` millisecond durations as an
    // `Instant` deadline. The deprecated key accepted those values, so retain load compatibility
    // with a deterministic, effectively unbounded fallback rather than rejecting old config.
    const ONE_YEAR_MS: u64 = 365 * 24 * 60 * 60 * 1_000;
    let mut fallback_ms = ONE_YEAR_MS;
    while fallback_ms > DEFAULT_YIELD_MS
        && now
            .checked_add(Duration::from_millis(fallback_ms))
            .is_none()
    {
        fallback_ms = (fallback_ms / 2).max(DEFAULT_YIELD_MS);
    }
    fallback_ms
}

impl ToolExecutionTimingRange {
    pub fn new(min_ms: u64, default_ms: u64, max_ms: u64) -> Result<Self, String> {
        let range = Self {
            min_ms,
            default_ms,
            max_ms,
        };
        range.validate("tool execution timing range")?;
        Ok(range)
    }

    pub const fn min_ms(self) -> u64 {
        self.min_ms
    }

    pub const fn default_ms(self) -> u64 {
        self.default_ms
    }

    pub const fn max_ms(self) -> u64 {
        self.max_ms
    }

    pub fn resolve_request(self, requested_ms: Option<u64>) -> ResolvedTiming {
        let Some(requested_ms) = requested_ms else {
            return ResolvedTiming {
                effective_ms: self.default_ms,
                adjustment: None,
            };
        };
        let effective_ms = requested_ms.clamp(self.min_ms, self.max_ms);
        let adjustment = (effective_ms != requested_ms).then_some(TimingAdjustment {
            requested_ms,
            effective_ms,
            bound: if requested_ms < self.min_ms {
                TimingBound::Minimum
            } else {
                TimingBound::Maximum
            },
        });
        ResolvedTiming {
            effective_ms,
            adjustment,
        }
    }

    fn resolve(
        name: &str,
        config: Option<&ToolExecutionTimingRangeToml>,
        defaults: Self,
        legacy_max_ms: Option<u64>,
    ) -> Result<Self, String> {
        let range = Self {
            min_ms: config
                .and_then(|range| range.min_ms)
                .unwrap_or(defaults.min_ms),
            default_ms: config
                .and_then(|range| range.default_ms)
                .unwrap_or(defaults.default_ms),
            max_ms: config
                .and_then(|range| range.max_ms)
                .or(legacy_max_ms)
                .unwrap_or(defaults.max_ms),
        };
        range.validate(name)?;
        Ok(range)
    }

    fn validate(self, name: &str) -> Result<(), String> {
        for (field, value) in [
            ("min_ms", self.min_ms),
            ("default_ms", self.default_ms),
            ("max_ms", self.max_ms),
        ] {
            if value == 0 {
                return Err(format!("{name}.{field} must be greater than 0"));
            }
            if Instant::now()
                .checked_add(Duration::from_millis(value))
                .is_none()
            {
                return Err(format!(
                    "{name}.{field} cannot be represented as a host wait deadline"
                ));
            }
        }
        if self.min_ms > self.default_ms {
            return Err(format!("{name}.min_ms must be at most {name}.default_ms"));
        }
        if self.default_ms > self.max_ms {
            return Err(format!("{name}.default_ms must be at most {name}.max_ms"));
        }
        Ok(())
    }

    fn to_toml(self) -> ToolExecutionTimingRangeToml {
        ToolExecutionTimingRangeToml {
            min_ms: Some(self.min_ms),
            default_ms: Some(self.default_ms),
            max_ms: Some(self.max_ms),
        }
    }
}
