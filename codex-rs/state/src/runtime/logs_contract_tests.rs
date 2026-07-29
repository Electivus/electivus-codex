use super::LogStore;
use super::test_support::unique_temp_dir;
use crate::LogEntry;
use crate::LogQuery;
use crate::LogRow;
use crate::SqliteConfig;
use crate::migrations::runtime_logs_migrator;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::sync::Arc;

async fn sqlite_replicas() -> anyhow::Result<(LogStore, LogStore, std::path::PathBuf)> {
    let codex_home = unique_temp_dir();
    tokio::fs::create_dir_all(&codex_home).await?;
    let sqlite = SqliteConfig::from_sqlite_home(AbsolutePathBuf::try_from(codex_home.clone())?);
    let migrator = runtime_logs_migrator();
    let writer = sqlite
        .open_logs_db(&migrator, /*telemetry_override*/ None)
        .await?;
    let reader = sqlite
        .open_logs_db(&migrator, /*telemetry_override*/ None)
        .await?;
    Ok((
        LogStore::from_sqlite(Arc::new(writer)),
        LogStore::from_sqlite(Arc::new(reader)),
        codex_home,
    ))
}

fn entry(ts: i64, message: &str) -> LogEntry {
    LogEntry {
        ts,
        ts_nanos: 0,
        level: "INFO".to_string(),
        target: "contract".to_string(),
        message: Some(message.to_string()),
        feedback_log_body: None,
        thread_id: Some("thread-1".to_string()),
        process_uuid: Some("process-1".to_string()),
        module_path: Some("contract::logs".to_string()),
        file: Some("contract.rs".to_string()),
        line: Some(15),
    }
}

pub(crate) async fn run_replica_visibility_contract(
    writer: &LogStore,
    reader: &LogStore,
) -> anyhow::Result<()> {
    writer.insert_log(&entry(/*ts*/ 10, "single")).await?;
    writer
        .insert_logs(&[entry(/*ts*/ 20, "batch-one"), entry(/*ts*/ 30, "batch-two")])
        .await?;

    assert_eq!(
        reader.query_logs(&LogQuery::default()).await?,
        vec![
            LogRow {
                id: 1,
                ts: 10,
                ts_nanos: 0,
                level: "INFO".to_string(),
                target: "contract".to_string(),
                message: Some("single".to_string()),
                thread_id: Some("thread-1".to_string()),
                process_uuid: Some("process-1".to_string()),
                file: Some("contract.rs".to_string()),
                line: Some(15),
            },
            LogRow {
                id: 2,
                ts: 20,
                ts_nanos: 0,
                level: "INFO".to_string(),
                target: "contract".to_string(),
                message: Some("batch-one".to_string()),
                thread_id: Some("thread-1".to_string()),
                process_uuid: Some("process-1".to_string()),
                file: Some("contract.rs".to_string()),
                line: Some(15),
            },
            LogRow {
                id: 3,
                ts: 30,
                ts_nanos: 0,
                level: "INFO".to_string(),
                target: "contract".to_string(),
                message: Some("batch-two".to_string()),
                thread_id: Some("thread-1".to_string()),
                process_uuid: Some("process-1".to_string()),
                file: Some("contract.rs".to_string()),
                line: Some(15),
            },
        ]
    );
    Ok(())
}

pub(crate) async fn run_filter_order_and_max_id_contract(
    writer: &LogStore,
    reader: &LogStore,
) -> anyhow::Result<()> {
    let mut first = entry(/*ts*/ 10, "ignored");
    first.module_path = Some("contract::ignored".to_string());
    first.file = Some("ignored.rs".to_string());

    let mut second = entry(/*ts*/ 20, "beta second");
    second.level = "warn".to_string();
    second.thread_id = Some("thread-2".to_string());
    second.process_uuid = Some("process-2".to_string());
    second.module_path = Some("contract::worker".to_string());
    second.file = Some("worker.rs".to_string());
    second.line = Some(20);

    let mut third = entry(/*ts*/ 30, "beta third");
    third.level = "ERROR".to_string();
    third.thread_id = None;
    third.process_uuid = Some("process-2".to_string());
    third.module_path = Some("contract::worker::nested".to_string());
    third.file = Some("nested.rs".to_string());
    third.line = Some(30);

    let mut fourth = entry(/*ts*/ 40, "too late");
    fourth.level = "WARN".to_string();
    fourth.module_path = Some("contract::worker".to_string());
    fourth.file = Some("worker.rs".to_string());

    writer.insert_logs(&[first, second, third, fourth]).await?;

    let query = LogQuery {
        levels_upper: vec!["WARN".to_string(), "ERROR".to_string()],
        from_ts: Some(15),
        to_ts: Some(35),
        module_like: vec!["worker".to_string()],
        file_like: vec![".rs".to_string()],
        thread_ids: vec!["thread-2".to_string()],
        search: Some("beta".to_string()),
        include_threadless: true,
        after_id: Some(1),
        limit: Some(2),
        descending: true,
    };
    assert_eq!(
        reader.query_logs(&query).await?,
        vec![
            LogRow {
                id: 3,
                ts: 30,
                ts_nanos: 0,
                level: "ERROR".to_string(),
                target: "contract".to_string(),
                message: Some("beta third".to_string()),
                thread_id: None,
                process_uuid: Some("process-2".to_string()),
                file: Some("nested.rs".to_string()),
                line: Some(30),
            },
            LogRow {
                id: 2,
                ts: 20,
                ts_nanos: 0,
                level: "warn".to_string(),
                target: "contract".to_string(),
                message: Some("beta second".to_string()),
                thread_id: Some("thread-2".to_string()),
                process_uuid: Some("process-2".to_string()),
                file: Some("worker.rs".to_string()),
                line: Some(20),
            },
        ]
    );
    assert_eq!(reader.max_log_id(&query).await?, 3);
    Ok(())
}

pub(crate) async fn run_feedback_contract(
    writer: &LogStore,
    reader: &LogStore,
) -> anyhow::Result<()> {
    let mut thread = entry(/*ts*/ 10, "message fallback is not selected");
    thread.feedback_log_body = Some("thread body".to_string());
    thread.process_uuid = Some("shared-process".to_string());

    let mut threadless = entry(/*ts*/ 20, "ignored fallback");
    threadless.level = "WARN".to_string();
    threadless.feedback_log_body = Some("process body\n".to_string());
    threadless.thread_id = None;
    threadless.process_uuid = Some("shared-process".to_string());

    let mut unrelated = entry(/*ts*/ 30, "unrelated");
    unrelated.level = "ERROR".to_string();
    unrelated.thread_id = Some("thread-2".to_string());
    unrelated.process_uuid = Some("other-process".to_string());

    writer.insert_logs(&[thread, threadless, unrelated]).await?;

    assert_eq!(
        reader.query_feedback_logs("thread-1").await?,
        b"1970-01-01T00:00:10.000000Z  INFO thread body\n\
1970-01-01T00:00:20.000000Z  WARN process body\n"
            .to_vec()
    );
    Ok(())
}

pub(crate) async fn run_startup_retention_contract(
    writer: &LogStore,
    reader: &LogStore,
) -> anyhow::Result<()> {
    let retained_ts = chrono::Utc::now().timestamp();
    writer
        .insert_logs(&[entry(/*ts*/ 1, "expired"), entry(retained_ts, "retained")])
        .await?;

    writer.run_startup_maintenance().await?;

    let rows = reader.query_logs(&LogQuery::default()).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].message.as_deref(), Some("retained"));

    writer.delete_logs_for_thread("thread-1").await?;
    assert_eq!(reader.query_logs(&LogQuery::default()).await?, Vec::new());
    Ok(())
}

pub(crate) async fn run_partition_limits_contract(
    writer: &LogStore,
    reader: &LogStore,
) -> anyhow::Result<()> {
    let entries = (0..=1_000)
        .map(|index| {
            let mut entry = entry(index, &format!("row-{index}"));
            if index % 2 == 0 {
                entry.process_uuid = None;
            }
            entry
        })
        .collect::<Vec<_>>();
    writer.insert_logs(&entries).await?;

    let thread_rows = reader
        .query_logs(&LogQuery {
            thread_ids: vec!["thread-1".to_string()],
            ..LogQuery::default()
        })
        .await?;
    assert_eq!(thread_rows.len(), 1_000);
    assert_eq!(thread_rows.first().map(|row| row.id), Some(2));
    assert_eq!(thread_rows.last().map(|row| row.id), Some(1_001));

    let mut process_entries = (0..=1_000)
        .map(|index| {
            let mut entry = entry(1_000 + index, &format!("process-row-{index}"));
            entry.thread_id = None;
            entry.process_uuid = Some("thread-1".to_string());
            entry
        })
        .collect::<Vec<_>>();
    process_entries.push(entry(/*ts*/ 2_001, "newest-thread-row"));
    writer.insert_logs(&process_entries).await?;
    let process_rows = reader
        .query_logs(&LogQuery {
            include_threadless: true,
            ..LogQuery::default()
        })
        .await?;
    assert_eq!(process_rows.len(), 1_000);
    assert_eq!(process_rows.first().map(|row| row.id), Some(1_003));
    assert_eq!(process_rows.last().map(|row| row.id), Some(2_002));
    let updated_thread_rows = reader
        .query_logs(&LogQuery {
            thread_ids: vec!["thread-1".to_string()],
            ..LogQuery::default()
        })
        .await?;
    assert_eq!(updated_thread_rows.len(), 1_000);
    assert_eq!(updated_thread_rows.first().map(|row| row.id), Some(3));
    assert_eq!(updated_thread_rows.last().map(|row| row.id), Some(2_003));

    let oversized_body = "x".repeat(10 * 1024 * 1024 + 1);
    let mut oversized = entry(/*ts*/ 2_000, &oversized_body);
    oversized.thread_id = Some("oversized-thread".to_string());
    writer.insert_log(&oversized).await?;
    assert_eq!(
        reader
            .query_logs(&LogQuery {
                thread_ids: vec!["oversized-thread".to_string()],
                ..LogQuery::default()
            })
            .await?,
        Vec::<LogRow>::new()
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_logs_satisfy_replica_visibility_contract() -> anyhow::Result<()> {
    let (writer, reader, codex_home) = sqlite_replicas().await?;
    let cleanup = scopeguard::guard(codex_home, |path| {
        let _ = std::fs::remove_dir_all(path);
    });

    run_replica_visibility_contract(&writer, &reader).await?;
    writer.close().await;
    reader.close().await;
    drop(cleanup);
    Ok(())
}

#[tokio::test]
async fn sqlite_logs_satisfy_filter_order_and_max_id_contract() -> anyhow::Result<()> {
    let (writer, reader, codex_home) = sqlite_replicas().await?;
    let cleanup = scopeguard::guard(codex_home, |path| {
        let _ = std::fs::remove_dir_all(path);
    });

    run_filter_order_and_max_id_contract(&writer, &reader).await?;
    writer.close().await;
    reader.close().await;
    drop(cleanup);
    Ok(())
}

#[tokio::test]
async fn sqlite_logs_satisfy_feedback_contract() -> anyhow::Result<()> {
    let (writer, reader, codex_home) = sqlite_replicas().await?;
    let cleanup = scopeguard::guard(codex_home, |path| {
        let _ = std::fs::remove_dir_all(path);
    });

    run_feedback_contract(&writer, &reader).await?;
    writer.close().await;
    reader.close().await;
    drop(cleanup);
    Ok(())
}

#[tokio::test]
async fn sqlite_logs_satisfy_startup_retention_contract() -> anyhow::Result<()> {
    let (writer, reader, codex_home) = sqlite_replicas().await?;
    let cleanup = scopeguard::guard(codex_home, |path| {
        let _ = std::fs::remove_dir_all(path);
    });

    run_startup_retention_contract(&writer, &reader).await?;
    writer.close().await;
    reader.close().await;
    drop(cleanup);
    Ok(())
}

#[tokio::test]
async fn sqlite_logs_satisfy_partition_limits_contract() -> anyhow::Result<()> {
    let (writer, reader, codex_home) = sqlite_replicas().await?;
    let cleanup = scopeguard::guard(codex_home, |path| {
        let _ = std::fs::remove_dir_all(path);
    });

    run_partition_limits_contract(&writer, &reader).await?;
    writer.close().await;
    reader.close().await;
    drop(cleanup);
    Ok(())
}
