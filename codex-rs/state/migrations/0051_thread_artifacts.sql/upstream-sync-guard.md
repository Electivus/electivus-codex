This directory deliberately conflicts with the rust-v0.152.0 migration of the same name.
Resolve the Catch-up by removing this guard and the incoming `0051` file; the migration is
pre-staged as `0054_thread_artifacts.sql` so existing fork migration versions remain immutable.
