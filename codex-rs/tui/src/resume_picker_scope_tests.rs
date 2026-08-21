use super::*;
use pretty_assertions::assert_eq;

#[test]
fn scope_defaults_follow_launch_context_and_cwd_availability() {
    use SessionFilterMode::All;
    use SessionFilterMode::Project;

    assert_eq!(
        SessionScope::project(Some(PathBuf::from("/tmp/project"))).mode(),
        Project
    );
    assert_eq!(
        SessionScope::all(Some(PathBuf::from("/tmp/project"))).mode(),
        All
    );
    assert_eq!(SessionScope::project(/*current_cwd*/ None).mode(), All);
}

#[test]
fn scope_transitions_cover_every_directed_edge() {
    use ScopeChangeDirection::Next;
    use ScopeChangeDirection::Previous;
    use SessionFilterMode::All;
    use SessionFilterMode::Cwd;
    use SessionFilterMode::Project;

    let transitions = [
        (Project, Next, Cwd),
        (Cwd, Next, All),
        (All, Next, Project),
        (Project, Previous, All),
        (All, Previous, Cwd),
        (Cwd, Previous, Project),
    ];

    for (current, direction, expected) in transitions {
        assert_eq!(
            current.changed(direction, /*has_current_cwd*/ true),
            expected
        );
    }
}

#[test]
fn thread_list_params_encode_each_scope_as_a_complete_request() {
    let cases = [
        (
            SessionLocationFilter::Project(PathBuf::from("/tmp/project")),
            None,
            Some(LegacyAppPathString::from_string("/tmp/project")),
        ),
        (
            SessionLocationFilter::Cwd(PathBuf::from("/tmp/project")),
            Some(ThreadListCwdFilter::One(String::from("/tmp/project"))),
            None,
        ),
        (SessionLocationFilter::All, None, None),
    ];

    for (location_filter, cwd, project_cwd) in cases {
        let params = ThreadListQuery {
            cursor: Some(String::from("cursor-1")),
            limit: 25,
            sort_key: ThreadSortKey::UpdatedAt,
            model_providers: Some(vec![String::from("openai")]),
            source_kinds: vec![ThreadSourceKind::Cli, ThreadSourceKind::VsCode],
            location_filter,
            archived: true,
            use_state_db_only: true,
        }
        .into_params();

        assert_eq!(
            params,
            ThreadListParams {
                cursor: Some(String::from("cursor-1")),
                limit: Some(25),
                sort_key: Some(ThreadSortKey::UpdatedAt),
                sort_direction: None,
                model_providers: Some(vec![String::from("openai")]),
                source_kinds: Some(vec![ThreadSourceKind::Cli, ThreadSourceKind::VsCode]),
                archived: Some(true),
                is_pinned: None,
                section_id: None,
                cwd,
                project_cwd,
                use_state_db_only: true,
                search_term: None,
                parent_thread_id: None,
                ancestor_thread_id: None,
            }
        );
    }
}
