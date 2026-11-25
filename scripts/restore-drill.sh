#!/bin/sh
# Backup/restore drill for the gateway database.
#
#   scripts/restore-drill.sh SOURCE_URL TARGET_URL [DUMP_FILE]
#
# Dumps the `gateway` schema of SOURCE_URL with pg_dump (custom format),
# restores it into TARGET_URL, and checks that every table holds the rows the
# dump holds and every identity sequence continues past its data. Counts
# come from the dump, which is one consistent snapshot, so the drill works
# against a gateway that keeps serving traffic. Both
# URLs connect as the application role (it owns the schema). The target
# database must already have an empty `gateway` schema owned by that role,
# as docker/postgres/001-create-gateway-app.sh creates; the drill refuses to
# restore over existing tables. The dump is kept when DUMP_FILE is given.
#
# Afterwards, start the gateway against TARGET_URL (its migrations are a
# no-op on a restored copy) and run the checks in the operations guide (https://gateway.qucelerate.com/operations/).
# Secrets restore sealed: they open only with the same
# GATEWAY_CREDENTIAL_ENCRYPTION_KEY or AWS KMS key as the source.
set -eu
# Byte order everywhere, so lists from psql and sort compare equal.
export LC_ALL=C

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    echo "usage: $0 SOURCE_URL TARGET_URL [DUMP_FILE]" >&2
    exit 2
fi
source_url=$1
target_url=$2
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
dump=${3:-$work/gateway.dump}

count_tables() {
    psql --no-psqlrc --quiet --tuples-only --no-align --dbname "$1" \
        --command "SELECT count(*) FROM pg_tables WHERE schemaname = 'gateway'"
}

# Row counts of every table, one `table count` line each, sorted.
table_counts() {
    psql --no-psqlrc --quiet --tuples-only --no-align --field-separator ' ' \
        --dbname "$1" <<'SQL'
SELECT format('SELECT %L, count(*) FROM gateway.%I', tablename, tablename)
FROM pg_tables WHERE schemaname = 'gateway' ORDER BY tablename
\gexec
SQL
}

echo "checking target is empty"
if [ "$(count_tables "$target_url")" != "0" ]; then
    echo "target already has gateway tables; refusing to restore over them" >&2
    exit 1
fi

echo "dumping source"
pg_dump --format=custom --schema=gateway --no-owner --no-privileges \
    --file "$dump" --dbname "$source_url"

echo "restoring into target"
# The schema exists already (owned by the application role, which cannot
# create schemas), so its own entry is left out.
pg_restore --list "$dump" > "$work/dump.list"
grep -v ' SCHEMA - gateway ' "$work/dump.list" > "$work/restore.list"
# The tables the dump's snapshot held, from its table of contents.
awk '$4 == "TABLE" && $5 == "gateway" { print $6 }' "$work/dump.list" \
    | sort > "$work/source.tables"
# Restored as SQL through psql, in one transaction. A client newer than the
# server emits `SET transaction_timeout` (PostgreSQL 17+), which an older
# server rejects; that session setting is dropped.
pg_restore --no-owner --no-privileges --use-list "$work/restore.list" \
    --file "$work/restore.sql" "$dump"
sed '/^SET transaction_timeout = /d' "$work/restore.sql" > "$work/restore.filtered.sql"
# Rows per table in the dump: COPY text format writes one line per row.
awk '
    /^COPY gateway\./ { split($2, name, "."); table = name[2]; rows = 0; copying = 1; next }
    copying && $0 == "\\." { print table, rows; copying = 0; next }
    copying { rows++ }
' "$work/restore.sql" | sort > "$work/source.counts"
cut -d' ' -f1 "$work/source.counts" > "$work/dumped.tables"
if ! diff -u "$work/source.tables" "$work/dumped.tables"; then
    echo "the dump does not hold the rows of every table it defines" >&2
    exit 1
fi
psql --no-psqlrc --quiet --set ON_ERROR_STOP=1 --single-transaction \
    --dbname "$target_url" --file "$work/restore.filtered.sql" > /dev/null

echo "verifying"
table_counts "$target_url" | sort > "$work/target.counts"
if ! diff -u "$work/source.counts" "$work/target.counts"; then
    echo "row counts differ between source and restored copy" >&2
    exit 1
fi
# Every identity column must continue past the restored rows, or the next
# insert would collide.
behind=$(psql --no-psqlrc --quiet --tuples-only --no-align --dbname "$target_url" <<'SQL'
SELECT format(
    'SELECT %L WHERE (SELECT last_value FROM %s) < coalesce((SELECT max(%I) FROM gateway.%I), 0)',
    c.table_name || '.' || c.column_name,
    pg_get_serial_sequence('gateway.' || quote_ident(c.table_name), c.column_name),
    c.column_name, c.table_name)
FROM information_schema.columns c
WHERE c.table_schema = 'gateway' AND c.is_identity = 'YES'
\gexec
SQL
)
if [ -n "$behind" ]; then
    echo "identity sequences behind their data: $behind" >&2
    exit 1
fi
migrations=$(psql --no-psqlrc --quiet --tuples-only --no-align --dbname "$target_url" \
    --command "SELECT count(*) || ' migrations, latest ' || max(version) FROM gateway._sqlx_migrations WHERE success")
tables=$(wc -l < "$work/target.counts" | tr -d ' ')
echo "restore drill passed: $tables tables match, $migrations"
