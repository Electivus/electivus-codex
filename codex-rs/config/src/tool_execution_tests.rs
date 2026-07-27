use pretty_assertions::assert_eq;

use crate::TimingBound;
use crate::ToolExecutionPolicy;
use crate::ToolExecutionTimingRange;
use crate::ToolExecutionTimingRangeToml;
use crate::ToolExecutionToml;

#[test]
fn partial_timing_overrides_resolve_before_requests_are_clamped() {
    let config = ToolExecutionToml {
        timeout: Some(ToolExecutionTimingRangeToml {
            min_ms: Some(20_000),
            max_ms: Some(900_000),
            ..Default::default()
        }),
        ..Default::default()
    };

    let policy = ToolExecutionPolicy::resolve(Some(&config), /*legacy_yield_max_ms*/ None).unwrap();

    assert_eq!(
        policy.timeout().resolve_request(Some(5_000)),
        crate::ResolvedTiming {
            effective_ms: 20_000,
            adjustment: Some(crate::TimingAdjustment {
                requested_ms: 5_000,
                effective_ms: 20_000,
                bound: TimingBound::Minimum,
            }),
        }
    );
    assert_eq!(
        policy.timeout().resolve_request(Some(1_000_000)),
        crate::ResolvedTiming {
            effective_ms: 900_000,
            adjustment: Some(crate::TimingAdjustment {
                requested_ms: 1_000_000,
                effective_ms: 900_000,
                bound: TimingBound::Maximum,
            }),
        }
    );
    assert_eq!(policy.timeout().resolve_request(None).effective_ms, 600_000);
    assert_eq!(
        policy.yield_time().resolve_request(None).effective_ms,
        30_000
    );
}

#[test]
fn legacy_background_timeout_maps_to_yield_maximum() {
    let policy = ToolExecutionPolicy::resolve(
        /*config*/ None,
        /*legacy_yield_max_ms*/ Some(90_000),
    )
    .unwrap();

    assert_eq!(
        policy.yield_time(),
        ToolExecutionTimingRange::new(
            /*min_ms*/ 10_000, /*default_ms*/ 30_000, /*max_ms*/ 90_000,
        )
        .expect("legacy timing range should be valid")
    );
}

#[test]
fn legacy_background_timeout_below_new_default_keeps_loading() {
    for legacy_yield_max_ms in [0, 5_000, 10_000] {
        let policy = ToolExecutionPolicy::resolve(/*config*/ None, Some(legacy_yield_max_ms))
            .expect("legacy timing config should continue loading");

        assert_eq!(
            policy.yield_time(),
            ToolExecutionTimingRange::new(
                /*min_ms*/ 10_000, /*default_ms*/ 30_000, /*max_ms*/ 30_000,
            )
            .expect("normalized legacy timing range should be valid")
        );
    }
}

#[test]
fn unrepresentable_legacy_background_timeout_keeps_loading() {
    let policy = ToolExecutionPolicy::resolve(
        /*config*/ None,
        /*legacy_yield_max_ms*/ Some(u64::MAX),
    )
    .expect("legacy timing config should continue loading");

    assert!(
        std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(
                policy.yield_time().max_ms()
            ))
            .is_some()
    );
}

#[test]
fn legacy_and_new_yield_maximum_conflict() {
    let config = ToolExecutionToml {
        yield_time: Some(ToolExecutionTimingRangeToml {
            max_ms: Some(90_000),
            ..Default::default()
        }),
        ..Default::default()
    };

    assert_eq!(
        ToolExecutionPolicy::resolve(Some(&config), /*legacy_yield_max_ms*/ Some(90_000)),
        Err(
            "`background_terminal_max_timeout` conflicts with `tool_execution.yield.max_ms`; remove the deprecated key"
                .to_string()
        )
    );
}

#[test]
fn invalid_timing_ranges_are_rejected() {
    let cases = [
        (
            ToolExecutionToml {
                timeout: Some(ToolExecutionTimingRangeToml {
                    min_ms: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "tool_execution.timeout.min_ms must be greater than 0",
        ),
        (
            ToolExecutionToml {
                timeout: Some(ToolExecutionTimingRangeToml {
                    default_ms: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "tool_execution.timeout.default_ms must be greater than 0",
        ),
        (
            ToolExecutionToml {
                timeout: Some(ToolExecutionTimingRangeToml {
                    max_ms: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "tool_execution.timeout.max_ms must be greater than 0",
        ),
        (
            ToolExecutionToml {
                timeout: Some(ToolExecutionTimingRangeToml {
                    min_ms: Some(700_000),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "tool_execution.timeout.min_ms must be at most tool_execution.timeout.default_ms",
        ),
        (
            ToolExecutionToml {
                timeout: Some(ToolExecutionTimingRangeToml {
                    max_ms: Some(100_000),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "tool_execution.timeout.default_ms must be at most tool_execution.timeout.max_ms",
        ),
        (
            ToolExecutionToml {
                yield_time: Some(ToolExecutionTimingRangeToml {
                    min_ms: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "tool_execution.yield.min_ms must be greater than 0",
        ),
        (
            ToolExecutionToml {
                yield_time: Some(ToolExecutionTimingRangeToml {
                    default_ms: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "tool_execution.yield.default_ms must be greater than 0",
        ),
        (
            ToolExecutionToml {
                yield_time: Some(ToolExecutionTimingRangeToml {
                    max_ms: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "tool_execution.yield.max_ms must be greater than 0",
        ),
        (
            ToolExecutionToml {
                yield_time: Some(ToolExecutionTimingRangeToml {
                    min_ms: Some(40_000),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "tool_execution.yield.min_ms must be at most tool_execution.yield.default_ms",
        ),
        (
            ToolExecutionToml {
                yield_time: Some(ToolExecutionTimingRangeToml {
                    max_ms: Some(20_000),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "tool_execution.yield.default_ms must be at most tool_execution.yield.max_ms",
        ),
    ];

    for (config, expected) in cases {
        assert_eq!(
            ToolExecutionPolicy::resolve(Some(&config), /*legacy_yield_max_ms*/ None),
            Err(expected.to_string())
        );
    }
}

#[test]
fn host_deadline_representation_is_validated_when_the_platform_has_a_finite_bound() {
    if std::time::Instant::now()
        .checked_add(std::time::Duration::from_millis(u64::MAX))
        .is_some()
    {
        return;
    }

    for (config, expected) in [
        (
            ToolExecutionToml {
                timeout: Some(ToolExecutionTimingRangeToml {
                    max_ms: Some(u64::MAX),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "tool_execution.timeout.max_ms cannot be represented as a host wait deadline",
        ),
        (
            ToolExecutionToml {
                yield_time: Some(ToolExecutionTimingRangeToml {
                    max_ms: Some(u64::MAX),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "tool_execution.yield.max_ms cannot be represented as a host wait deadline",
        ),
    ] {
        assert_eq!(
            ToolExecutionPolicy::resolve(Some(&config), /*legacy_yield_max_ms*/ None),
            Err(expected.to_string())
        );
    }
}
