# Database Schema Documentation

This service uses PostgreSQL. Schema and seed scripts are in:

- `services/api/database/migrations/`
- `services/api/database/seeds/`

## Tables

- `newsletter_subscribers` — email opt-in list with double-opt-in confirmation
- `contact_form_submissions`
- `waitlist_entries`
- `analytics_events`
- `content_management`
- `audit_logs` — general audit trail (UUID primary key)
- `audit_log` — append-only admin-operation audit log (bigserial primary key)
- `email_jobs` — async email queue tracking

## Migration Files

1. `000_create_schema_migrations.sql`
2. `001_enable_pgcrypto.sql`
3. `002_create_newsletter_subscriptions.sql` — creates `newsletter_subscribers` table
4. `003_create_contact_form_submissions.sql`
5. `004_create_waitlist_entries.sql`
6. `005_create_content_management.sql`
7. `006_create_analytics_events.sql`
8. `007_create_audit_logs.sql`
9. `008_create_email_tracking.sql`
10. `009_add_newsletter_indexes.sql` — performance indexes on `newsletter_subscribers`
11. `010_add_soft_delete_newsletter.sql` — adds `deleted_at` to `newsletter_subscribers`
12. `010_create_audit_log.sql` — append-only `audit_log` table for admin operations
13. `011_create_markets.sql` — creates `markets` table
14. `012_add_performance_indexes.sql` — composite indexes on `markets` and `content` (promoted from `sql/`)

> **Note:** Two migration files share the `010_` prefix. Apply them in lexicographic
> order (`010_add_soft_delete_newsletter.sql` before `010_create_audit_log.sql`) or
> rename one to `011_` to avoid ambiguity with migration runners that sort by filename.

## sql/ Directory

`services/api/sql/` contains **query templates and ad-hoc reference SQL** — not schema migrations.

| File | Purpose |
|---|---|
| `performance_indexes.sql` | Source for the indexes now in `012_add_performance_indexes.sql`. Kept as a reference; do not apply manually. |
| `newsletter_schema.sql` | Early draft of the `newsletter_subscribers` schema. Superseded by `002_create_newsletter_subscriptions.sql`. Do not apply manually. |

> **Rule:** No schema-altering SQL should be applied from `sql/` directly. All schema changes must go through a numbered migration in `database/migrations/`.

## Connection Pool Configuration

Pool sizing and timeouts are fully env-configurable — no code changes needed for different deployment sizes.

| Variable | Default | Description |
|---|---|---|
| `DB_POOL_MIN_CONNECTIONS` | `5` | Minimum idle connections kept open |
| `DB_POOL_MAX_CONNECTIONS` | `25` | Maximum concurrent connections |
| `DB_POOL_ACQUIRE_TIMEOUT_SECS` | `5` | Seconds to wait for a free connection before error |
| `DB_POOL_IDLE_TIMEOUT_SECS` | _(sqlx default)_ | Seconds before idle connections are reaped (0 = disabled) |
| `DB_POOL_MAX_LIFETIME_SECS` | _(sqlx default)_ | Max lifetime of a connection in seconds (0 = disabled) |
| `DB_QUERY_TIMEOUT_SECS` | `30` | Per-query execution timeout; queries exceeding this return an error |

**Sizing guidance:**
- Small / dev: `DB_POOL_MIN_CONNECTIONS=2 DB_POOL_MAX_CONNECTIONS=5`
- Medium: `DB_POOL_MIN_CONNECTIONS=5 DB_POOL_MAX_CONNECTIONS=25` (default)
- Large / high-traffic: `DB_POOL_MIN_CONNECTIONS=10 DB_POOL_MAX_CONNECTIONS=100`

Pool metrics are exposed on the `/metrics` Prometheus endpoint under the `db_pool_*` family.

## Apply Migrations

Run from the workspace root:

```bash
for f in services/api/database/migrations/*.sql; do
  psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f "$f"
done
```

Or use the provided script:

```bash
bash services/api/scripts/run_migrations.sh
```

## Rollback

This repository uses forward-only SQL migrations. For rollback:

- Write explicit reverse scripts before production rollout.
- Restore from backup/snapshot for emergency rollback.

## Seeding

```bash
for f in services/api/database/seeds/*.sql; do
  psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f "$f"
done
```

## Backup Strategy

- Daily logical backups with `pg_dump`, 30-day retention.
- Weekly full snapshot, 90-day retention.
- Quarterly restore drills in staging.
- Encrypt backup storage at rest.

## Data Retention Policy

- `analytics_events`: 13 months raw, then archive/aggregate.
- `audit_logs` / `audit_log`: 24 months minimum for compliance.
- `contact_form_submissions`: 12 months unless legal hold.
- `newsletter_subscribers` / `waitlist_entries`: retain active records; hard-delete on GDPR request.

## Notes

- UUID primary keys via `gen_random_uuid()` (most tables); `audit_log` uses `BIGSERIAL`.
- All tables include `created_at` / `updated_at` timestamps.
- Soft deletes via `deleted_at` in `content_management`, `audit_logs`, and `newsletter_subscribers`.
- Indexes on high-frequency query fields (`email`, `status`, `created_at`).
