ALTER TABLE threads
ADD COLUMN history_projection_version BIGINT CHECK (history_projection_version >= 0);

CREATE FUNCTION invalidate_thread_history_projection() RETURNS TRIGGER AS $$
BEGIN
    EXECUTE format(
        'UPDATE %I.threads SET history_projection_version = NULL WHERE thread_id = $1',
        TG_TABLE_SCHEMA
    ) USING CASE WHEN TG_OP = 'DELETE' THEN OLD.thread_id ELSE NEW.thread_id END;
    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER thread_items_invalidate_history_projection
AFTER INSERT OR UPDATE OR DELETE ON thread_items
FOR EACH ROW EXECUTE FUNCTION invalidate_thread_history_projection();

CREATE TRIGGER thread_turns_invalidate_history_projection
AFTER INSERT OR UPDATE OR DELETE ON thread_turns
FOR EACH ROW EXECUTE FUNCTION invalidate_thread_history_projection();
