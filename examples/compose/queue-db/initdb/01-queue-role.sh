#!/bin/sh
# First-init only: create the unprivileged application role and hand it the
# queue database. The bootstrap superuser is never used by clients.
set -eu
psql -v ON_ERROR_STOP=1 -v app_pw="$QUEUE_DB_APP_PASSWORD" \
  --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" <<'SQL'
CREATE ROLE queue_app LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD :'app_pw';
ALTER DATABASE queue OWNER TO queue_app;
REVOKE ALL ON DATABASE queue FROM PUBLIC;
GRANT CONNECT ON DATABASE queue TO queue_app;
ALTER SCHEMA public OWNER TO queue_app;
SQL
