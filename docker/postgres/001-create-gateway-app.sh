#!/bin/sh
set -eu

# The official image runs this script as the bootstrap POSTGRES_USER during
# first initialization. Compose supplies these values only for local use.
psql \
  --username "$POSTGRES_USER" \
  --dbname "$POSTGRES_DB" \
  --set=gateway_app_user="$GATEWAY_APP_USER" \
  --set=gateway_app_password="$GATEWAY_APP_PASSWORD" \
  --set ON_ERROR_STOP=1 <<'SQL'
SELECT format(
    'CREATE ROLE %I LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT PASSWORD %L',
    :'gateway_app_user',
    :'gateway_app_password'
)
WHERE NOT EXISTS (
    SELECT 1 FROM pg_roles WHERE rolname = :'gateway_app_user'
)
\gexec

SELECT format(
    'ALTER ROLE %I LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT PASSWORD %L',
    :'gateway_app_user',
    :'gateway_app_password'
)
\gexec

SELECT format('CREATE SCHEMA IF NOT EXISTS gateway AUTHORIZATION %I', :'gateway_app_user')
\gexec

SELECT format('ALTER SCHEMA gateway OWNER TO %I', :'gateway_app_user')
\gexec
SQL
