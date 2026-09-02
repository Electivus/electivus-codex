This directory deliberately conflicts with the rust-v0.152.0 migration of the same name.
Resolve the Catch-up by removing this guard and the incoming `0052` file; the migration is
pre-staged as `0055_projects_recency.sql` so it runs after the fork's `0052_projects.sql`.
