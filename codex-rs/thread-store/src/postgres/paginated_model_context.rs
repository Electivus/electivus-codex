use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionMetaLine;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanProgress;
use codex_rollout::ModelContextScanSignal;
use codex_rollout::RolloutItem;
use futures::TryStreamExt;
use sqlx::AssertSqlSafe;
use sqlx::Row;
use sqlx::Transaction;

use super::MAX_MODEL_CONTEXT_BYTES;
use super::MAX_MODEL_CONTEXT_ITEMS;
use super::ModelContextBudget;
use super::bounded_item_from_row;
use super::database_error;
use super::validate_model_context_item;
use crate::ThreadStoreResult;

const MODEL_CONTEXT_PAGE_ITEMS: usize = 64;

pub(super) async fn load(
    history_table: &str,
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    thread_id: ThreadId,
    session_meta: SessionMetaLine,
    base_budget: ModelContextBudget,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let cutoff_ordinal =
        scan_cutoff_ordinal(history_table, transaction, thread_id, &base_budget).await?;
    let project_item_completed = projection_required(
        history_table,
        transaction,
        thread_id,
        cutoff_ordinal,
        &base_budget,
    )
    .await?;
    load_selected_items(
        history_table,
        transaction,
        thread_id,
        cutoff_ordinal,
        project_item_completed,
        session_meta,
        base_budget,
    )
    .await
}

async fn scan_cutoff_ordinal(
    history_table: &str,
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    thread_id: ThreadId,
    budget: &ModelContextBudget,
) -> ThreadStoreResult<i64> {
    let mut scan = ModelContextScan::default();
    let mut rows = sqlx::query(AssertSqlSafe(format!(
        "WITH candidates AS ( \
             SELECT ordinal, item ->> 'type' AS rollout_type, \
                    item #>> '{{payload,type}}' AS payload_type, \
                    item #>> '{{payload,turn_id}}' AS turn_id, \
                    item #>> '{{payload,item,type}}' AS event_item_type, \
                    CASE WHEN item ->> 'type' = 'response_item' \
                                   AND item #>> '{{payload,type}}' = 'message' \
                                   AND item #>> '{{payload,role}}' = 'assistant' \
                                   AND CASE \
                                       WHEN jsonb_typeof(item #> '{{payload,content}}') = 'array' \
                                       THEN jsonb_array_length(item #> '{{payload,content}}') = 1 \
                                       ELSE false END \
                                   AND item #>> '{{payload,content,0,type}}' \
                                       IN ('input_text', 'output_text') \
                                   AND jsonb_typeof(item #> '{{payload,content,0,text}}') = 'string' \
                              THEN item #>> '{{payload,content,0,text}}' END \
                        AS inter_agent_message_text, \
                    COALESCE(jsonb_typeof(item #> '{{payload,replacement_history}}') \
                             <> 'null', false) AS has_replacement_history, \
                    COALESCE((jsonb_typeof(item #> '{{payload,window_number}}') <> 'null') \
                             OR jsonb_typeof(item #> '{{payload,window_id}}') = 'number', \
                             false) \
                        AS has_window_number \
             FROM {history_table} WHERE thread_id = $1 AND ordinal > 0 \
             ORDER BY ordinal DESC LIMIT $2 \
         ), sized AS ( \
             SELECT *, \
                    (octet_length(COALESCE(rollout_type, '')) \
                     + octet_length(COALESCE(payload_type, '')) \
                     + octet_length(COALESCE(turn_id, '')) \
                     + octet_length(COALESCE(event_item_type, '')) \
                     + octet_length(COALESCE(inter_agent_message_text, '')) + 64)::bigint \
                        AS scan_bytes, \
                    GREATEST(octet_length(COALESCE(rollout_type, '')), \
                             octet_length(COALESCE(payload_type, '')), \
                             octet_length(COALESCE(turn_id, '')), \
                             octet_length(COALESCE(event_item_type, '')), \
                             octet_length(COALESCE(inter_agent_message_text, '')))::bigint \
                        AS max_scan_field_bytes \
             FROM candidates \
         ), bounded AS ( \
             SELECT *, SUM(scan_bytes) OVER (ORDER BY ordinal DESC)::bigint \
                           AS cumulative_scan_bytes \
             FROM sized \
         ) \
         SELECT ordinal, \
                CASE WHEN max_scan_field_bytes <= $3 AND cumulative_scan_bytes <= $3 \
                     THEN rollout_type END AS rollout_type, \
                CASE WHEN max_scan_field_bytes <= $3 AND cumulative_scan_bytes <= $3 \
                     THEN payload_type END AS payload_type, \
                CASE WHEN max_scan_field_bytes <= $3 AND cumulative_scan_bytes <= $3 \
                     THEN turn_id END AS turn_id, \
                CASE WHEN max_scan_field_bytes <= $3 AND cumulative_scan_bytes <= $3 \
                     THEN event_item_type END AS event_item_type, \
                CASE WHEN max_scan_field_bytes <= $3 AND cumulative_scan_bytes <= $3 \
                     THEN inter_agent_message_text END AS inter_agent_message_text, \
                has_replacement_history, has_window_number, \
                max_scan_field_bytes > $3 AS scan_field_too_large, \
                cumulative_scan_bytes > $3 AS scan_budget_exceeded \
         FROM bounded ORDER BY ordinal DESC"
    )))
    .bind(thread_id.to_string())
    .bind(i64::try_from(MAX_MODEL_CONTEXT_ITEMS).unwrap_or(i64::MAX))
    .bind(
        i64::try_from(MAX_MODEL_CONTEXT_BYTES.saturating_sub(budget.bytes)).unwrap_or(i64::MAX),
    )
    .fetch(transaction.as_mut());
    while let Some(row) = rows
        .try_next()
        .await
        .map_err(|error| database_error("scan latest model context", error))?
    {
        let scan_field_too_large: bool = row
            .try_get("scan_field_too_large")
            .map_err(|error| database_error("scan latest model context", error))?;
        if scan_field_too_large {
            return Err(budget.limit_error("an individual history scan field is too large"));
        }
        let scan_budget_exceeded: bool = row
            .try_get("scan_budget_exceeded")
            .map_err(|error| database_error("scan latest model context", error))?;
        if scan_budget_exceeded {
            return Err(
                budget.limit_error("history structural scan exceeds the bounded read budget")
            );
        }
        let rollout_type: Option<String> = row
            .try_get("rollout_type")
            .map_err(|error| database_error("scan latest model context", error))?;
        let payload_type: Option<String> = row
            .try_get("payload_type")
            .map_err(|error| database_error("scan latest model context", error))?;
        let turn_id: Option<String> = row
            .try_get("turn_id")
            .map_err(|error| database_error("scan latest model context", error))?;
        let Some(signal) = signal_from_row(
            &row,
            rollout_type.as_deref().unwrap_or_default(),
            payload_type.as_deref(),
            turn_id,
            budget,
        )?
        else {
            continue;
        };
        if matches!(scan.push_signal(signal), ModelContextScanProgress::Complete) {
            let ordinal = row
                .try_get("ordinal")
                .map_err(|error| database_error("scan latest model context", error))?;
            drop(rows);
            return Ok(ordinal);
        }
    }
    drop(rows);
    Ok(1)
}

fn signal_from_row(
    row: &sqlx::postgres::PgRow,
    rollout_type: &str,
    payload_type: Option<&str>,
    turn_id: Option<String>,
    budget: &ModelContextBudget,
) -> ThreadStoreResult<Option<ModelContextScanSignal>> {
    let signal = match (rollout_type, payload_type) {
        ("compacted", _) => ModelContextScanSignal::Compacted {
            has_replacement_history: row
                .try_get("has_replacement_history")
                .map_err(|error| database_error("scan latest model context", error))?,
            has_window_number: row
                .try_get("has_window_number")
                .map_err(|error| database_error("scan latest model context", error))?,
        },
        ("event_msg", Some("thread_rolled_back")) => ModelContextScanSignal::ThreadRolledBack,
        ("event_msg", Some("item_completed")) => {
            let event_item_type: Option<String> = row
                .try_get("event_item_type")
                .map_err(|error| database_error("scan latest model context", error))?;
            ModelContextScanSignal::ItemCompleted {
                turn_id: turn_id.ok_or_else(|| {
                    budget.limit_error("item_completed scan event has no turn id")
                })?,
                is_user_message: event_item_type.as_deref() == Some("UserMessage"),
            }
        }
        ("event_msg", Some("task_complete" | "turn_complete")) => {
            ModelContextScanSignal::TurnComplete {
                turn_id: turn_id
                    .ok_or_else(|| budget.limit_error("turn_complete scan event has no turn id"))?,
            }
        }
        ("event_msg", Some("turn_aborted")) => ModelContextScanSignal::TurnAborted { turn_id },
        ("event_msg", Some("task_started" | "turn_started")) => {
            ModelContextScanSignal::TurnStarted {
                turn_id: turn_id
                    .ok_or_else(|| budget.limit_error("turn_started scan event has no turn id"))?,
            }
        }
        ("event_msg", Some("user_message")) => ModelContextScanSignal::UserMessage,
        ("turn_context", _) => ModelContextScanSignal::TurnContext { turn_id },
        ("response_item", Some("agent_message")) => ModelContextScanSignal::ResponseItem {
            counts_as_user_turn: true,
        },
        ("response_item", Some("message")) => {
            let message_text: Option<String> = row
                .try_get("inter_agent_message_text")
                .map_err(|error| database_error("scan latest model context", error))?;
            let is_inter_agent_message = message_text
                .is_some_and(|text| serde_json::from_str::<InterAgentCommunication>(&text).is_ok());
            ModelContextScanSignal::ResponseItem {
                counts_as_user_turn: is_inter_agent_message,
            }
        }
        ("response_item", _) => ModelContextScanSignal::ResponseItem {
            counts_as_user_turn: false,
        },
        ("inter_agent_communication", _) => ModelContextScanSignal::InterAgentCommunication,
        _ => return Ok(None),
    };
    Ok(Some(signal))
}

async fn projection_required(
    history_table: &str,
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    thread_id: ThreadId,
    cutoff_ordinal: i64,
    budget: &ModelContextBudget,
) -> ThreadStoreResult<bool> {
    let remaining_items = budget.max_items.saturating_sub(budget.items);
    let sentinel_limit = remaining_items.saturating_add(1);
    let item_count: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT COUNT(*)::bigint FROM ( \
             SELECT ordinal FROM {history_table} \
             WHERE thread_id = $1 AND ordinal >= $2 \
             ORDER BY ordinal ASC LIMIT $3 \
         ) AS bounded_suffix"
    )))
    .bind(thread_id.to_string())
    .bind(cutoff_ordinal)
    .bind(i64::try_from(sentinel_limit).unwrap_or(i64::MAX))
    .fetch_one(transaction.as_mut())
    .await
    .map_err(|error| database_error("count latest model context", error))?;
    let item_count =
        usize::try_from(item_count).map_err(|_| budget.limit_error("invalid item count"))?;
    if item_count > remaining_items {
        return Err(budget.limit_error("history exceeds the bounded read budget"));
    }

    let summary = sqlx::query(AssertSqlSafe(format!(
        "WITH bounded_suffix AS ( \
             SELECT item FROM {history_table} \
             WHERE thread_id = $1 AND ordinal >= $2 \
             ORDER BY ordinal ASC LIMIT $3 \
         ) \
         SELECT COALESCE(SUM(octet_length(item::text)), 0)::bigint AS raw_bytes, \
                COALESCE(SUM(CASE \
                    WHEN item ->> 'type' = 'event_msg' \
                         AND item #>> '{{payload,type}}' = 'item_completed' \
                    THEN octet_length(COALESCE(item #>> '{{payload,turn_id}}', '')) \
                       + octet_length(COALESCE(item #>> '{{payload,item,type}}', '')) + 32 \
                    ELSE octet_length(item::text) END), 0)::bigint AS projected_bytes \
         FROM bounded_suffix"
    )))
    .bind(thread_id.to_string())
    .bind(cutoff_ordinal)
    .bind(i64::try_from(item_count).unwrap_or(i64::MAX))
    .fetch_one(transaction.as_mut())
    .await
    .map_err(|error| database_error("summarize latest model context", error))?;
    let raw_bytes = summary_bytes(&summary, "raw_bytes", budget)?;
    let projected_bytes = summary_bytes(&summary, "projected_bytes", budget)?;
    let remaining_bytes = MAX_MODEL_CONTEXT_BYTES.saturating_sub(budget.bytes);
    let projection_required = raw_bytes > remaining_bytes;
    let selected_bytes = if projection_required {
        projected_bytes
    } else {
        raw_bytes
    };
    if selected_bytes > remaining_bytes {
        return Err(budget.limit_error("history exceeds the bounded read budget"));
    }
    Ok(projection_required)
}

fn summary_bytes(
    row: &sqlx::postgres::PgRow,
    column: &str,
    budget: &ModelContextBudget,
) -> ThreadStoreResult<u64> {
    let bytes: i64 = row
        .try_get(column)
        .map_err(|error| database_error("summarize latest model context", error))?;
    u64::try_from(bytes).map_err(|_| budget.limit_error("invalid history size"))
}

async fn load_selected_items(
    history_table: &str,
    transaction: &mut Transaction<'_, sqlx::Postgres>,
    thread_id: ThreadId,
    cutoff_ordinal: i64,
    project_item_completed: bool,
    session_meta: SessionMetaLine,
    mut budget: ModelContextBudget,
) -> ThreadStoreResult<Vec<RolloutItem>> {
    let mut items = vec![RolloutItem::SessionMeta(session_meta)];
    let mut next_ordinal = cutoff_ordinal;
    loop {
        let mut rows = sqlx::query(AssertSqlSafe(format!(
            "WITH selected AS ( \
                 SELECT ordinal, item, octet_length(item::text)::bigint AS raw_bytes, \
                        $2::boolean AND item ->> 'type' = 'event_msg' \
                            AND item #>> '{{payload,type}}' = 'item_completed' AS scan_event, \
                        item #>> '{{payload,turn_id}}' AS event_turn_id, \
                        item #>> '{{payload,item,type}}' AS event_item_type \
                 FROM {history_table} WHERE thread_id = $1 AND ordinal >= $4 \
                 ORDER BY ordinal ASC LIMIT $5 \
             ), sized AS ( \
                 SELECT *, (octet_length(COALESCE(event_turn_id, '')) \
                            + octet_length(COALESCE(event_item_type, '')) + 32)::bigint \
                               AS scan_bytes \
                 FROM selected \
             ) \
             SELECT ordinal, CASE WHEN NOT scan_event AND raw_bytes <= $3 THEN item END AS item, \
                    scan_event, event_turn_id AS scan_turn_id, \
                    CASE WHEN scan_event THEN scan_bytes ELSE raw_bytes END AS item_bytes \
             FROM sized ORDER BY ordinal ASC"
        )))
        .bind(thread_id.to_string())
        .bind(project_item_completed)
        .bind(i64::try_from(MAX_MODEL_CONTEXT_BYTES).unwrap_or(i64::MAX))
        .bind(next_ordinal)
        .bind(i64::try_from(MODEL_CONTEXT_PAGE_ITEMS).unwrap_or(i64::MAX))
        .fetch(transaction.as_mut());
        let mut page_items = 0;
        let mut next_page_ordinal = None;
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|error| database_error("load latest model context", error))?
        {
            page_items += 1;
            let ordinal: i64 = row
                .try_get("ordinal")
                .map_err(|error| database_error("load latest model context", error))?;
            next_page_ordinal = ordinal.checked_add(1);
            let item_bytes: i64 = row
                .try_get("item_bytes")
                .map_err(|error| database_error("load latest model context", error))?;
            budget.account_item(item_bytes)?;
            let scan_event: bool = row
                .try_get("scan_event")
                .map_err(|error| database_error("load latest model context", error))?;
            if scan_event {
                let turn_id: Option<String> = row
                    .try_get("scan_turn_id")
                    .map_err(|error| database_error("load latest model context", error))?;
                if turn_id.is_none() {
                    return Err(budget.limit_error("item_completed scan event has no turn id"));
                }
                continue;
            }
            let item = bounded_item_from_row(&row, &budget)?;
            if !matches!(&item, RolloutItem::SessionMeta(_)) {
                validate_model_context_item(&item, &mut budget)?;
            }
            items.push(item);
        }
        drop(rows);
        if page_items < MODEL_CONTEXT_PAGE_ITEMS {
            break;
        }
        let Some(ordinal) = next_page_ordinal else {
            break;
        };
        next_ordinal = ordinal;
    }
    Ok(items)
}
