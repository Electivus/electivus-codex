use codex_config::ToolExecutionTimingRange;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(crate) fn create_wait_tool(yield_time: ToolExecutionTimingRange) -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "cell_id".to_string(),
            JsonSchema::string(Some("Identifier of the running exec cell.".to_string())),
        ),
        (
            "yield_time_ms".to_string(),
            JsonSchema::number_with_bounds(
                Some(format!(
                    "Wait before yielding more output. Configured default is {} ms; effective range is {}-{} ms.",
                    yield_time.default_ms(),
                    yield_time.min_ms(),
                    yield_time.max_ms()
                )),
                yield_time.min_ms(),
                yield_time.max_ms(),
            ),
        ),
        (
            "max_tokens".to_string(),
            JsonSchema::number(Some(
                "Output token budget for this wait call. Defaults to 10000 tokens.".to_string(),
            )),
        ),
        (
            "terminate".to_string(),
            JsonSchema::boolean(Some(
                "True stops the running exec cell; false or omitted waits for output.".to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: codex_code_mode::WAIT_TOOL_NAME.to_string(),
        description: format!(
            "Waits on a yielded `{}` cell and returns new output or completion.\n{}",
            codex_code_mode::PUBLIC_TOOL_NAME,
            codex_code_mode::build_wait_tool_description().trim()
        ),
        strict: false,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["cell_id".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
        defer_loading: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn create_wait_tool_matches_expected_spec() {
        let yield_time = ToolExecutionTimingRange::new(
            /*min_ms*/ 111, /*default_ms*/ 222, /*max_ms*/ 333,
        )
        .expect("test yield range should be valid");
        assert_eq!(
            create_wait_tool(yield_time),
            ToolSpec::Function(ResponsesApiTool {
                name: codex_code_mode::WAIT_TOOL_NAME.to_string(),
                description: format!(
                    "Waits on a yielded `{}` cell and returns new output or completion.\n{}",
                    codex_code_mode::PUBLIC_TOOL_NAME,
                    codex_code_mode::build_wait_tool_description().trim()
                ),
                strict: false,
                defer_loading: None,
                parameters: JsonSchema::object(
                    BTreeMap::from([
                        (
                            "cell_id".to_string(),
                            JsonSchema::string(Some(
                                "Identifier of the running exec cell.".to_string()
                            )),
                        ),
                        (
                            "max_tokens".to_string(),
                            JsonSchema::number(Some(
                                "Output token budget for this wait call. Defaults to 10000 tokens."
                                    .to_string(),
                            )),
                        ),
                        (
                            "terminate".to_string(),
                            JsonSchema::boolean(Some(
                                "True stops the running exec cell; false or omitted waits for output."
                                    .to_string(),
                            )),
                        ),
                        (
                            "yield_time_ms".to_string(),
                            JsonSchema::number_with_bounds(
                                Some(
                                    "Wait before yielding more output. Configured default is 222 ms; effective range is 111-333 ms.".to_string(),
                                ),
                                /*minimum*/ 111,
                                /*maximum*/ 333,
                            ),
                        ),
                    ]),
                    Some(vec!["cell_id".to_string()]),
                    Some(false.into()),
                ),
                output_schema: None,
            })
        );
    }
}
