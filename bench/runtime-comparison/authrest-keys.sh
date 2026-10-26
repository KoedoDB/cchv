#!/bin/bash
# Makes what rtbench_authrest.py needs, in the directory given: made-up keys and passwords for this run (nothing real), the settings of Auth and PostgREST as the Control Plane gives them
# (control-plane/internal/agentcontrol/dbservices.go), the SQL that prepares the database, and a token with the role anon. The database address is the placeholder @DB@ (filled in at the run).
# Usage: authrest-keys.sh DIR PATH_TO_koedodb-public
set -euo pipefail
d="${1:?dir}"; pub="${2:?path to koedodb-public}"; H="$pub/auth/tests/jwt_helper.py"
mkdir -p "$d"; chmod 700 "$d"
"$H" genkey bench-1 "$d/k.pem" "$d/k.priv.json" "$d/k.pub.json"
jwks="$(python3 -c "import sys, json; print(json.dumps({'keys': [json.load(open(sys.argv[1]))]}, separators=(',', ':')))" "$d/k.pub.json")"
secret="$(openssl rand -hex 32)"
cat > "$d/auth.env" <<ENV
GOTRUE_API_HOST=0.0.0.0
PORT=9999
API_EXTERNAL_URL=http://localhost:9999
GOTRUE_SITE_URL=http://localhost:3000
GOTRUE_DB_DRIVER=postgres
DATABASE_URL=postgres://supabase_auth_admin:authpw@@DB@:5432/postgres
DB_NAMESPACE=auth
GOTRUE_JWT_SECRET=$secret
GOTRUE_JWT_EXP=3600
GOTRUE_JWT_AUD=authenticated
GOTRUE_JWT_ISSUER=https://bench.example/auth/v1
GOTRUE_JWT_ADMIN_ROLES=service_role
GOTRUE_JWT_DEFAULT_GROUP_NAME=authenticated
GOTRUE_MAILER_AUTOCONFIRM=true
GOTRUE_DISABLE_SIGNUP=false
GOTRUE_LOG_LEVEL=info
GOTRUE_JWT_KEYS=[$(python3 -c "import json,sys; print(json.dumps(json.load(open(sys.argv[1])), separators=(',', ':')))" "$d/k.priv.json")]
ENV
cat > "$d/rest.env" <<ENV
PGRST_DB_URI=postgres://authenticator:restpw@@DB@:5432/postgres
PGRST_DB_SCHEMAS=public
PGRST_JWT_SECRET=$jwks
PGRST_JWT_AUD=authenticated
PGRST_DB_MAX_ROWS=1000
PGRST_SERVER_PORT=3000
PGRST_ADMIN_SERVER_PORT=3001
ENV
cat > "$d/init.sql" <<'SQL'
CREATE USER supabase_auth_admin NOINHERIT CREATEROLE LOGIN NOREPLICATION PASSWORD 'authpw';
CREATE SCHEMA IF NOT EXISTS auth AUTHORIZATION supabase_auth_admin;
GRANT CREATE ON DATABASE postgres TO supabase_auth_admin;
ALTER USER supabase_auth_admin SET search_path = 'auth';
CREATE ROLE anon NOLOGIN NOINHERIT;
CREATE ROLE authenticated NOLOGIN NOINHERIT;
CREATE ROLE service_role NOLOGIN NOINHERIT BYPASSRLS;
CREATE ROLE authenticator NOINHERIT LOGIN PASSWORD 'restpw';
GRANT anon, authenticated, service_role TO authenticator;
GRANT USAGE ON SCHEMA public TO anon, authenticated, service_role;
CREATE TABLE public.items (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name text NOT NULL, note text NOT NULL);
INSERT INTO public.items (name, note) SELECT 'item ' || g, repeat('x', 100) FROM generate_series(1, 1000) g;
GRANT SELECT ON public.items TO anon, authenticated, service_role;
SQL
"$H" sign "$d/k.pem" bench-1 '{"role":"anon","aud":"authenticated","exp":4102444800}' > "$d/anon.jwt"
echo "keys and settings in $d"
