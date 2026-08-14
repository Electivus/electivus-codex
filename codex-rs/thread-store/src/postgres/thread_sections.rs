use super::PostgresThreadStore;
use crate::CreateThreadSectionParams;
use crate::DeleteThreadSectionParams;
use crate::ListThreadSectionsParams;
use crate::RenameThreadSectionParams;
use crate::StoredThreadSection;
use crate::StoredThreadSectionsPage;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

pub(super) async fn list_thread_sections(
    store: &PostgresThreadStore,
    params: ListThreadSectionsParams,
) -> ThreadStoreResult<StoredThreadSectionsPage> {
    let page = state_db(store, "threadSection/list")?
        .list_thread_sections(params.cursor.as_deref(), params.limit)
        .await
        .map_err(|error| section_error("list", error))?;
    Ok(StoredThreadSectionsPage {
        sections: page
            .sections
            .into_iter()
            .map(|section| StoredThreadSection {
                id: section.id,
                name: section.name,
                appearance: section.appearance,
            })
            .collect(),
        next_cursor: page.next_cursor,
    })
}

pub(super) async fn create_thread_section(
    store: &PostgresThreadStore,
    params: CreateThreadSectionParams,
) -> ThreadStoreResult<StoredThreadSection> {
    let section = state_db(store, "threadSection/create")?
        .create_thread_section(&params.name, params.appearance)
        .await
        .map_err(|error| section_error("create", error))?;
    Ok(StoredThreadSection {
        id: section.id,
        name: section.name,
        appearance: section.appearance,
    })
}

pub(super) async fn rename_thread_section(
    store: &PostgresThreadStore,
    params: RenameThreadSectionParams,
) -> ThreadStoreResult<Option<StoredThreadSection>> {
    state_db(store, "threadSection/update")?
        .rename_thread_section(&params.section_id, &params.name, params.appearance)
        .await
        .map(|section| {
            section.map(|section| StoredThreadSection {
                id: section.id,
                name: section.name,
                appearance: section.appearance,
            })
        })
        .map_err(|error| section_error("update", error))
}

pub(super) async fn delete_thread_section(
    store: &PostgresThreadStore,
    params: DeleteThreadSectionParams,
) -> ThreadStoreResult<bool> {
    state_db(store, "threadSection/delete")?
        .delete_thread_section(&params.section_id)
        .await
        .map_err(|error| section_error("delete", error))
}

fn state_db<'store>(
    store: &'store PostgresThreadStore,
    operation: &'static str,
) -> ThreadStoreResult<&'store codex_state::StateRuntime> {
    store
        .state_db
        .as_deref()
        .ok_or(ThreadStoreError::Unsupported { operation })
}

fn section_error(operation: &str, error: impl std::fmt::Display) -> ThreadStoreError {
    ThreadStoreError::Internal {
        message: format!("failed to {operation} thread section: {error}"),
    }
}
