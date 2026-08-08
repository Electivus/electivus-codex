use super::*;
use pretty_assertions::assert_eq;

#[test]
fn model_context_budget_rejects_unbounded_item_counts_and_bytes() {
    let thread_id = codex_protocol::ThreadId::from_string("0198c4cf-8587-7d32-8d1c-2c14d331f038")
        .expect("thread id");
    let mut item_limited =
        ModelContextBudget::with_limits(thread_id, /*max_items*/ 2, /*max_bytes*/ 100);
    item_limited
        .account_item(/*item_bytes*/ 10)
        .expect("first item");
    item_limited
        .account_item(/*item_bytes*/ 10)
        .expect("second item");
    let item_error = item_limited
        .account_item(/*item_bytes*/ 10)
        .expect_err("third item");

    let mut byte_limited =
        ModelContextBudget::with_limits(thread_id, /*max_items*/ 10, /*max_bytes*/ 20);
    byte_limited
        .account_item(/*item_bytes*/ 20)
        .expect("exact byte budget");
    let byte_error = byte_limited
        .account_item(/*item_bytes*/ 1)
        .expect_err("byte budget overflow");

    assert_eq!(
        [item_error.to_string(), byte_error.to_string()],
        [
            format!(
                "invalid thread-store request: model context for thread {thread_id} cannot be loaded safely: history exceeds the bounded read budget (limit: 2 items or 100 bytes)"
            ),
            format!(
                "invalid thread-store request: model context for thread {thread_id} cannot be loaded safely: history exceeds the bounded read budget (limit: 10 items or 20 bytes)"
            ),
        ]
    );
}
