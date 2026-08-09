use codex_config::ResolvedTiming;
use codex_config::TimingBound;
use codex_config::ToolExecutionPolicy;
use codex_config::ToolExecutionTimingRange;

#[derive(Debug, Clone, Copy)]
pub(crate) enum TimingParameter {
    Timeout,
    Yield,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum YieldTimingClass {
    Global,
    InteractiveExec,
    InteractiveStdin,
}

pub(crate) fn resolve_yield_timing(
    policy: ToolExecutionPolicy,
    class: YieldTimingClass,
    requested_ms: Option<u64>,
) -> ResolvedTiming {
    let range = match class {
        YieldTimingClass::Global => policy.yield_time(),
        YieldTimingClass::InteractiveExec => fixed_timing_range(
            /*min_ms*/ 250, /*default_ms*/ 10_000, /*max_ms*/ 30_000,
        ),
        YieldTimingClass::InteractiveStdin => fixed_timing_range(
            /*min_ms*/ 250, /*default_ms*/ 250, /*max_ms*/ 30_000,
        ),
    };
    range.resolve_request(requested_ms)
}

fn fixed_timing_range(min_ms: u64, default_ms: u64, max_ms: u64) -> ToolExecutionTimingRange {
    match ToolExecutionTimingRange::new(min_ms, default_ms, max_ms) {
        Ok(range) => range,
        Err(message) => unreachable!("fixed timing range is invalid: {message}"),
    }
}

pub(crate) fn adjustment_message(
    parameter: TimingParameter,
    timing: ResolvedTiming,
) -> Option<String> {
    let adjustment = timing.adjustment?;
    let parameter = match parameter {
        TimingParameter::Timeout => "timeout_ms",
        TimingParameter::Yield => "yield_time_ms",
    };
    let bound = match adjustment.bound {
        TimingBound::Minimum => "minimum",
        TimingBound::Maximum => "maximum",
    };
    Some(format!(
        "Timing policy adjusted {parameter} from {} ms to {} ms ({bound} {} ms).",
        adjustment.requested_ms, adjustment.effective_ms, adjustment.effective_ms
    ))
}

pub(crate) fn error_with_timing_adjustment(
    parameter: TimingParameter,
    timing: ResolvedTiming,
    error: impl std::fmt::Display,
) -> String {
    let error = error.to_string();
    match adjustment_message(parameter, timing) {
        Some(message) => format!("{message}\n{error}"),
        None => error,
    }
}
