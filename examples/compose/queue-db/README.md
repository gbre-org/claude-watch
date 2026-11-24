# queue-db: dedicated Postgres for the session-task queue

Queue-only Postgres (`postgres:16-alpine`), its own volume, published on host
loopback only (`127.0.0.1:5434`). Backups: a sidecar writes a gzipped
`pg_dump` daily to `QUEUE_DB_BACKUP_DIR` (default retention 14 days).

## Secrets (never committed)

- `~/.config/queue-db/queue-db.env` (mode 0600): `QUEUE_DB_ADMIN_PASSWORD`,
  `QUEUE_DB_APP_PASSWORD`, `QUEUE_DB_PORT`, `QUEUE_DB_BACKUP_DIR`.
- `~/.config/session/pg-dsn` (mode 0600): `postgresql://queue_app:<pw>@auto:5434/queue`
  (`auto` = `127.0.0.1` on the host, `host.docker.internal` in the container).

Clients use the unprivileged `queue_app` role (owns only the `queue` database).

## Start / stop (only this project; never touches other stacks)

    docker compose --env-file ~/.config/queue-db/queue-db.env \
      -f examples/compose/queue-db/docker-compose.yml up -d

## Cutover runbook (JSON -> Postgres)

Let `Q=~/.config/session`.

1. Preflight: `pg_isready -h 127.0.0.1 -p 5434` and `session-task queue pg-migrate --dry-run`
   (prints the item count; it does not contact Postgres).
2. Backup: `cp -p $Q/queue.json $Q/queue.json.pre-cutover.$(date -u +%Y%m%dT%H%M%SZ)`.
3. Quiesce: tell running agents not to mutate the queue; the main loop stops
   enqueueing/`done`/`abandon` until step 7. Mutations after step 4 are lost
   from the Postgres copy (re-run step 4 with `--force` if needed before step 5).
4. Migrate: `session-task queue pg-migrate` (also creates the schema; refuses to
   overwrite a populated row without `--force`; writes its own `queue.json.pre-pg-migrate.*`).
5. Flip: `echo postgres > $Q/backend` (every process picks it up on its next
   command; no restart needed).
6. Verify: `session-task queue list` and `session-task queue show <id>` match the
   pre-flip state; container and host both see the same data; run one
   `session-task queue heartbeat` or equivalent write and read it back from the
   other side.
7. Resume agents.

Rollback: `session-task queue pg-export-json` (writes the Postgres state back to
`queue.json`), then `rm $Q/backend`.
Export before deleting the file so JSON carries mutations made since the flip.

## Restore a backup

    gunzip -c <backups>/queue-<ts>.sql.gz | docker exec -i queue-db-queue-db-1 \
      psql -U queue_admin -d queue

(Restore into an empty database; drop and recreate `queue` first if populated.)
