#!/bin/sh
set -eu

# Optional: the least-privilege role `data-plane` processes connect as. Runs
# after 001 during first initialization when GATEWAY_DATA_PLANE_PASSWORD is
# set. The role gets no privileges here; `gateway migrate` grants exactly the
# data plane's access whenever it runs (the operations guide (https://gateway.qucelerate.com/operations/)).
if [ -z "${GATEWAY_DATA_PLANE_PASSWORD:-}" ]; then
  exit 0
fi

psql \
  --username "$POSTGRES_USER" \
  --dbname "$POSTGRES_DB" \
  --set=data_plane_password="$GATEWAY_DATA_PLANE_PASSWORD" \
  --set ON_ERROR_STOP=1 <<'SQL'
SELECT 'CREATE ROLE gateway_data_plane LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT'
WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'gateway_data_plane')
\gexec

SELECT format(
    'ALTER ROLE gateway_data_plane LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT PASSWORD %L',
    :'data_plane_password'
)
\gexec
SQL
