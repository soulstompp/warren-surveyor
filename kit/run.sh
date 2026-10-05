#!/usr/bin/env bash
# Copyright (c) 2026 Kenneth Allen Flegal
# SPDX-License-Identifier: MIT OR Apache-2.0
#
# The whole kit in one command, from this clone, on the machine your PostgreSQL 18 server runs on:
#
#   kit/run.sh --db postgres://USER@HOST:5432/lego --pg-config /path/to/pg_config
#
# It checks everything first and changes nothing until every check holds. Then it builds the bench,
# installs the surveyor's extension into that server, makes the database --db names (it refuses
# one that exists), loads the LEGO catalogue and the trapped database into it, records every
# answer with PostgreSQL alone, times every question with PostgreSQL alone, turns the surveyor on,
# times every question again with it and without it, turns it off, and prints where the results
# are. Everything it writes stays in this clone, but the extension's files, which go where the
# server loads extensions from, and the database it makes.
#
# Options:
#   --size small|medium|huge   the generator's size (default small; huge took us 36 min and 197 GB)
#   --surveyor-only            time only the surveyor's side: no PostgreSQL-alone run, no dba set
#   --sudo                     install the extension with sudo (a server installed by a package)
#   --no-memory-watchdog       for a server whose backends this machine cannot see in /proc
#   --out DIR                  where the results go (default runs/<size>-<UTC time> in this clone)

set -Eeuo pipefail

usage() { sed -n '5,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
say() { printf '\n=== %s %s\n' "$(date +%H:%M:%S)" "$*"; }
die() { printf 'kit: %s\n' "$*" >&2; exit 2; }

REPO=$(cd "$(dirname "$0")/.." && pwd)
DB_URL='' PG_CONFIG='' SIZE=small SURVEYOR_ONLY=0 SUDO='' WATCH='' OUT=''
while [ $# -gt 0 ]; do
    case "$1" in
        --db) DB_URL=${2:?--db needs a URL}; shift 2 ;;
        --pg-config) PG_CONFIG=${2:?--pg-config needs a path}; shift 2 ;;
        --size) SIZE=${2:?--size needs small, medium or huge}; shift 2 ;;
        --surveyor-only) SURVEYOR_ONLY=1; shift ;;
        --sudo) SUDO=--sudo; shift ;;
        --no-memory-watchdog) WATCH=--no-memory-watchdog; shift ;;
        --out) OUT=${2:?--out needs a directory}; shift 2 ;;
        -h|--help) usage ;;
        *) die "unknown option $1 (see kit/run.sh --help)" ;;
    esac
done
[ -n "$DB_URL" ] && [ -n "$PG_CONFIG" ] || usage
case "$SIZE" in small) NEED_GB=12 ;; medium) NEED_GB=60 ;; huge) NEED_GB=220 ;;
    *) die "--size is small, medium or huge, not $SIZE" ;; esac

# The URL: postgres://USER[:PASSWORD]@HOST:PORT/DATABASE[?…]. The database is the one the kit makes.
case "$DB_URL" in postgres://*|postgresql://*) ;; *) die "--db must be a postgres:// URL" ;; esac
url_path=${DB_URL%%\?*}
DB_NAME=${url_path##*/}
BASE=${url_path%/*}
case "$DB_NAME" in ''|*[!a-z0-9_]*|[0-9]*) die "--db must end in a plain database name (a-z, 0-9, _), not '$DB_NAME'" ;; esac
DB="$BASE/$DB_NAME"
ADMIN="$BASE/postgres"
TARGET="$BASE/$DB_NAME?options=-c%20search_path%3Dlego%2Cpublic"
shown() { printf '%s' "$1" | sed 's#://\([^:/@]*\):[^@]*@#://\1:***@#'; }

export CARGO_TARGET_DIR="$REPO/target"
export PGRX_HOME="$REPO/target/kit-pgrx"
unset DATABASE_URL PGOPTIONS PGDATABASE PGSERVICE || true
B="$REPO/target/release/warren-bench"
TOOLS="$REPO/target/kit-tools"
OUT=${OUT:-"$REPO/runs/$SIZE-$(date -u +%Y%m%dT%H%M%SZ)"}
q() { psql -X -At -v ON_ERROR_STOP=1 "$@"; }

STEP='checks' SURVEYOR_ON=0
stopped() {
    printf '\nkit: stopped at: %s\n' "$STEP" >&2
    if [ "$SURVEYOR_ON" = 1 ]; then
        printf 'kit: the surveyor is on for %s; to turn it off:\n  %s kit surveyor-sql | psql -X -d %s -v mode=off -v apply=1 -f -\n' \
            "$DB_NAME" "$B" "$(shown "$DB")" >&2
    fi
}
trap stopped ERR

# 0. Checks: nothing changes until every one holds.
say "checks: tools, the server, the database $DB_NAME"
for tool in psql curl cargo; do
    command -v "$tool" >/dev/null || die "$tool is not on PATH"
done
[ -x "$PG_CONFIG" ] || die "--pg-config $PG_CONFIG is not an executable pg_config"
pg_version=$("$PG_CONFIG" --version)
case "$pg_version" in "PostgreSQL 18."*) ;; *) die "$PG_CONFIG is $pg_version; the surveyor needs PostgreSQL 18" ;; esac
cargo pgrx --version 2>/dev/null | grep -q '0\.19\.2' \
    || die "cargo-pgrx 0.19.2 is needed: cargo install cargo-pgrx --version 0.19.2 --locked"
server=$(q -d "$ADMIN" -c "SELECT current_setting('server_version') || '|' || (SELECT rolsuper::text FROM pg_roles WHERE rolname = current_user)") \
    || die "cannot connect to $(shown "$ADMIN")"
server_version=${server%%|*}
[ "${server#*|}" = true ] || die "the URL's user is not a superuser: the extension and the preload setting need one"
server_number=${server_version%% *}
pg_number=$(printf '%s' "$pg_version" | awk '{print $2}')
[ "$server_number" = "$pg_number" ] \
    || die "the server is $server_version but $PG_CONFIG is $pg_version: give the pg_config of the server --db names, on its machine"
contrib=$(q -d "$ADMIN" -c "SELECT count(*) FROM pg_available_extensions WHERE name IN ('cube', 'earthdistance', 'pg_trgm')")
[ "$contrib" = 3 ] || die "the server lacks cube, earthdistance or pg_trgm: install PostgreSQL 18's contrib package"
exists=$(q -d "$ADMIN" -c "SELECT count(*) FROM pg_database WHERE datname = '$DB_NAME'")
[ "$exists" = 0 ] || die "the database $DB_NAME exists: the kit makes its own and touches no other; name a new one in --db, or drop it yourself"
data_dir=$(q -d "$ADMIN" -c "SHOW data_directory")
free_gb=$(df --output=avail -B1G "$data_dir" 2>/dev/null | tail -1 | tr -d ' ' || true)
if [ -n "$free_gb" ] && [ "$free_gb" -eq "$free_gb" ] 2>/dev/null; then
    [ "$free_gb" -ge "$NEED_GB" ] || die "$data_dir has $free_gb GB free; a $SIZE load needs about $NEED_GB"
    printf 'the server data directory has %s GB free (a %s load needs about %s)\n' "$free_gb" "$SIZE" "$NEED_GB"
else
    printf 'cannot see %s from here: make sure it has about %s GB free\n' "$data_dir" "$NEED_GB"
fi
mkdir -p "$OUT"
fact() { printf '%s\t%s\n' "$1" "$2" | tee -a "$OUT/facts.txt"; }
fact date "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
fact commit "$(git -C "$REPO" log -1 --format='%h %s' 2>/dev/null || echo unknown)"
fact server "PostgreSQL $server_version"
fact size "$SIZE"
printf 'every check holds; results go to %s\n' "$OUT"

# Install: the bench in this clone, the generator, the extension into the server.
STEP='install'
say "install: the bench, in this clone"
cargo build --release --locked -p warren-bench --manifest-path "$REPO/Cargo.toml"
fact bench "$("$B" --version)"
if command -v sqlc-brickgen >/dev/null; then
    GEN=$(command -v sqlc-brickgen)
else
    say "install: the generator, sqlc-brickgen, from crates.io into $TOOLS"
    cargo install --locked sqlc-brickgen --root "$TOOLS"
    GEN="$TOOLS/bin/sqlc-brickgen"
fi
if [ "$GEN" = "$TOOLS/bin/sqlc-brickgen" ]; then
    fact generator "$(cargo install --list --root "$TOOLS" | grep '^sqlc-brickgen ')"
else
    fact generator "$GEN, already on PATH"
fi
say "install: the extension, into the server's own directories"
mkdir -p "$PGRX_HOME"
printf '[configs]\npg18 = "%s"\n' "$PG_CONFIG" > "$PGRX_HOME/config.toml"
(cd "$REPO/warren-surveyor-pg" && cargo pgrx install --release --pg-config "$PG_CONFIG" $SUDO)
fact extension "$(q -d "$ADMIN" -c "SELECT name || ' ' || default_version FROM pg_available_extensions WHERE name = 'warren_surveyor_pg'")"

# 1. Load.
STEP='1. load the catalogue and the trapped database'
say "$STEP"
q -d "$ADMIN" -c "CREATE DATABASE \"$DB_NAME\"" >/dev/null
curl -sSfL https://raw.githubusercontent.com/neondatabase/postgres-sample-dbs/main/lego.sql \
    | psql -X -q -v ON_ERROR_STOP=1 -d "$DB" >/dev/null
tables=$(q -d "$DB" -c "SELECT count(*) FROM pg_tables WHERE schemaname = 'public' AND tablename LIKE 'lego\_%'")
[ "$tables" = 8 ] || die "the catalogue's download left $tables tables, not 8"
started=$SECONDS
"$GEN" --database-url "$DB" --size "$SIZE" --oo-schema lego_oo --partitioning inheritance,range,hash \
    > "$OUT/generator.log" 2>&1
fact load_seconds $((SECONDS - started))
fact database_size "$(q -d "$DB" -c "SELECT pg_size_pretty(pg_database_size(current_database()))")"

surveyor() { "$B" kit surveyor-sql | psql -X -d "$DB" -v mode="$1" "${@:2}" -f -; }

# 2. Off, 3. record, 4. PostgreSQL alone timed.
STEP='2. turn the surveyor off'
say "$STEP"
surveyor off > "$OUT/off-plan.txt"
surveyor off -v apply=1 | tail -1
STEP='3. record the answers with PostgreSQL alone'
say "$STEP"
started=$SECONDS
"$B" truth --target lego lego.lego_purchases "$TARGET" --out "$OUT/expected.parquet" $WATCH
fact truth_seconds $((SECONDS - started))

worst=0
timed() {
    local name=$1 target=$2 sets=$3 rc=0 started=$SECONDS
    "$B" run --index-sets "$sets" --target "$target" lego.lego_purchases "$TARGET" \
        --expected "$OUT/expected.parquet" --out "$OUT/$name" $WATCH > "$OUT/$name.out" 2>&1 || rc=$?
    fact "${name}_seconds" $((SECONDS - started))
    fact "${name}_exit" "$rc"
    [ "$rc" -le "$worst" ] || worst=$rc
}
if [ "$SURVEYOR_ONLY" = 0 ]; then
    STEP='4. time every question with PostgreSQL alone'
    say "$STEP"
    timed alone alone all
fi

# 5. On, 6. run, then off again.
STEP='5. turn the surveyor on'
say "$STEP"
surveyor on > "$OUT/on-plan.txt"
fact surveyors_made "$(grep -c '^CREATE INDEX' "$OUT/on-plan.txt" || true)"
fact tables_named_with_none "$(grep -cE '^-- [^ ]+\.[^ ]+: ' "$OUT/on-plan.txt" || true)"
SURVEYOR_ON=1
surveyor on -v apply=1 | tail -1
STEP='6. run every question, with the surveyor and without it'
say "$STEP"
if [ "$SURVEYOR_ONLY" = 1 ]; then timed run lego all; else timed run lego dba,all; fi
STEP='turn the surveyor off again'
say "$STEP"
surveyor off -v apply=1 | tail -1
SURVEYOR_ON=0

# 7. Read.
say "7. read the difference"
for name in alone run; do
    [ -f "$OUT/$name/summary.txt" ] || continue
    sed -n '/^Per target/,$p' "$OUT/$name/summary.txt" | head -4
done
printf '\nevery file is under %s:\n' "$OUT"
printf '  %s\n' "run/summary.txt (and alone/summary.txt): per question and index set, pages first, then time" \
    "run/scans.txt: each scan's estimated rows beside its actual rows" \
    "facts.txt: what ran, on what, and how long each step took"
printf 'the database %s stays, with the surveyor off; to drop it: psql -X -d %s -c '"'"'DROP DATABASE %s'"'"'\n' \
    "$DB_NAME" "$(shown "$ADMIN")" "$DB_NAME"
exit "$worst"
