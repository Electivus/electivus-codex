CREATE TABLE remote_control_enrollments (
    websocket_url TEXT NOT NULL,
    account_id TEXT NOT NULL,
    app_server_client_name TEXT NOT NULL,
    server_id TEXT NOT NULL,
    environment_id TEXT NOT NULL,
    server_name TEXT NOT NULL,
    remote_control_enabled BOOLEAN,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (websocket_url, account_id, app_server_client_name)
);
