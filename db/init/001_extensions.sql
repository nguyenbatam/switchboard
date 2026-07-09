-- Runs once, on first initialization of an empty Postgres data volume.
-- Re-running requires `docker compose down -v`.

CREATE EXTENSION IF NOT EXISTS vector;
