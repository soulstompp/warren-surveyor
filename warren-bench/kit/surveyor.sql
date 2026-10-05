-- Copyright (c) 2026 Kenneth Allen Flegal
-- SPDX-License-Identifier: MIT OR Apache-2.0

-- Turns the surveyor on or off for a whole database, for psql. `warren-bench kit surveyor-sql`
-- prints this file: pipe it into psql with `-f -` in place of `-f surveyor.sql`.
--
--   psql -X -d lego -v mode=on  -f surveyor.sql            prints what turning it on would do
--   psql -X -d lego -v mode=on  -v apply=1 -f surveyor.sql  does it, in one transaction, then checks
--   psql -X -d lego -v mode=off -f surveyor.sql            prints what turning it off would do
--   psql -X -d lego -v mode=off -v apply=1 -f surveyor.sql  does it, in one transaction, then checks
--
-- -v schemas=lego,lego_oo limits it to those schemas; every schema but the system ones by default.
--
-- On: the extension warren_surveyor_pg; every new session of the database preloading its library
-- beside every library it preloads already (the database's own list, else the server's); one
-- surveyor on every table, on the table's own unique key, its primary key first, else the one with
-- the fewest columns; a partition takes its partitioned table's key, and its surveyor is made before
-- the partitioned table's, which then takes it as its own; ANALYZE on each table given a surveyor
-- that no table given one lies under (a partitioned table's ANALYZE reaches every partition). A table
-- with no unique key, with a key column of a type the surveyor has no class for, or that already has
-- a surveyor, gets none, and is named.
--
-- Off: every surveyor dropped (a partitioned table's takes its partitions' with it), and the library
-- taken out of the database's preload, every other library kept. The extension stays installed;
-- with no surveyor and no preload, every session plans with the DBA's indexes alone.
--
-- Both take each table's lock for as long as the transaction lasts: off drops indexes, which waits
-- for every query on the table. Run it when the database is quiet.
--
-- Setting session_preload_libraries on a database takes a superuser, or a role granted SET on it.
-- Sessions already open keep the libraries they started with: reconnect after either side.

\set ON_ERROR_STOP on
\if :{?mode}
\else
\echo 'usage: psql -X -d <database> -v mode=on|off [-v apply=1] [-v schemas=a,b] -f surveyor.sql'
\quit
\endif
\if :{?apply}
\else
\set apply 0
\endif
\if :{?schemas}
\else
\set schemas ''
\endif

SELECT :'mode' IN ('on', 'off') AS mode_known, :'mode' = 'on' AS turning_on,
       :'apply' IN ('1', 'on', 'true') AS applying,
       EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'warren_surveyor_pg') AS has_extension \gset
\if :mode_known
\else
DO $$ BEGIN RAISE EXCEPTION 'mode must be on or off'; END $$;
\endif

\set QUIET on
\pset tuples_only on
\pset format unaligned
-- the schemas chosen, and the libraries new sessions of the database preload, one row each
CREATE TEMP VIEW surveyor_schemas AS
SELECT n.oid, n.nspname
FROM pg_namespace n
WHERE n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast')
  AND n.nspname NOT LIKE 'pg\_temp\_%' AND n.nspname NOT LIKE 'pg\_toast\_temp\_%'
  AND (:'schemas' = '' OR n.nspname = ANY (string_to_array(replace(:'schemas', ' ', ''), ',')));
CREATE TEMP VIEW surveyor_preload_list AS
WITH own AS (
    SELECT (SELECT substr(c, length('session_preload_libraries=') + 1)
            FROM pg_db_role_setting r, unnest(r.setconfig) c
            WHERE r.setdatabase = d.oid AND r.setrole = 0
              AND c LIKE 'session\_preload\_libraries=%') AS libraries
    FROM pg_database d WHERE d.datname = current_database()
)
SELECT own.libraries IS NOT NULL AS own,
       coalesce(own.libraries, CASE WHEN s.source = 'configuration file' THEN s.setting END, '') AS libraries
FROM own, pg_settings s WHERE s.name = 'session_preload_libraries';
CREATE TEMP VIEW surveyor_preload AS
SELECT u.at, u.library, u.library ~ '(^|/)warren_surveyor_pg(\.so)?$' AS ours
FROM surveyor_preload_list l,
     LATERAL (SELECT m.at, CASE WHEN m.item[1] LIKE '"%'
                                THEN replace(substr(m.item[1], 2, length(m.item[1]) - 2), '""', '"')
                                ELSE btrim(m.item[1]) END AS library
              FROM regexp_matches(l.libraries, '"(?:[^"]|"")*"|[^,"]+', 'g') WITH ORDINALITY m(item, at)) u
WHERE u.library <> '';
\set QUIET off
SELECT format('-- %s the surveyor on database %s, schemas: %s; %s', CASE WHEN :'turning_on'::boolean THEN 'turning on' ELSE 'turning off' END,
              current_database(), coalesce(nullif(:'schemas', ''), 'every one but the system schemas'),
              CASE WHEN :'applying'::boolean THEN 'applying' ELSE 'printing only (add -v apply=1 to apply)' END);

\if :turning_on
SELECT NOT EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'warren_surveyor_pg') AS missing \gset
\if :missing
\if :applying
DO $$ BEGIN RAISE EXCEPTION 'warren_surveyor_pg is not installed in this server: build and install it first (see the README)'; END $$;
\else
\echo '-- warren_surveyor_pg is not installed in this server yet: build and install it before applying'
\endif
\endif
\endif

\if :applying
BEGIN;
SET LOCAL lock_timeout = '30s';
\if :turning_on
\if :has_extension
\else
-- made first, so that each key's types are checked against its classes
\echo 'CREATE EXTENSION warren_surveyor_pg;'
CREATE EXTENSION warren_surveyor_pg;
\endif
\endif
\endif

\set QUIET on
\if :turning_on
CREATE TEMP TABLE surveyor_plan AS
WITH RECURSIVE tables AS (
    SELECT c.oid, s.nspname, c.relname,
           CASE WHEN c.relispartition THEN pg_partition_root(c.oid) END AS root
    FROM pg_class c
    JOIN surveyor_schemas s ON s.oid = c.relnamespace
    WHERE c.relkind IN ('r', 'p')
      AND NOT EXISTS (SELECT 1 FROM pg_depend d
                      WHERE d.classid = 'pg_class'::regclass AND d.objid = c.oid AND d.deptype = 'e')
),
own_key AS (
    SELECT DISTINCT ON (x.indrelid) x.indrelid AS oid, x.indexrelid, x.indnkeyatts, x.indkey, x.indclass
    FROM pg_index x
    WHERE x.indisunique AND x.indisvalid AND x.indpred IS NULL
      AND x.indrelid IN (SELECT oid FROM tables UNION SELECT root FROM tables WHERE root IS NOT NULL)
    ORDER BY x.indrelid, x.indisprimary DESC, x.indnkeyatts, x.indexrelid::regclass::text
),
key_of AS (
    SELECT t.oid, coalesce(r.indexrelid, o.indexrelid) AS indexrelid,
           coalesce(r.indnkeyatts, o.indnkeyatts) AS indnkeyatts,
           coalesce(r.indkey, o.indkey) AS indkey,
           coalesce(r.indclass, o.indclass) AS indclass
    FROM tables t
    LEFT JOIN own_key r ON r.oid = t.root
    LEFT JOIN own_key o ON o.oid = t.oid
),
ordered AS (
    SELECT t.oid, t.nspname, t.relname, k.indexrelid,
           (SELECT string_agg(CASE WHEN k.indkey[i - 1] = 0
                                   THEN '(' || pg_get_indexdef(k.indexrelid, i, false) || ')'
                                   ELSE pg_get_indexdef(k.indexrelid, i, false) END,
                              ', ' ORDER BY i)
            FROM generate_series(1, k.indnkeyatts) i) AS columns,
           (SELECT string_agg(DISTINCT format_type(c.opcintype, NULL), ', ')
            FROM generate_series(1, k.indnkeyatts) i
            JOIN pg_opclass c ON c.oid = k.indclass[i - 1]
            WHERE NOT EXISTS (SELECT 1 FROM pg_opclass s JOIN pg_am a ON a.oid = s.opcmethod
                              WHERE a.amname = 'surveyor' AND s.opcdefault AND s.opcintype = c.opcintype)
           ) AS unclassed,
           t.relname || '_surveyor' AS surveyor,
           (SELECT count(*) FROM pg_partition_ancestors(t.oid)) AS depth
    FROM tables t
    JOIN key_of k ON k.oid = t.oid
),
why_none AS (
    SELECT o.oid,
           CASE
               WHEN EXISTS (SELECT 1 FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid
                            JOIN pg_am a ON a.oid = i.relam
                            WHERE x.indrelid = o.oid AND a.amname = 'surveyor')
                   THEN 'already has a surveyor'
               WHEN o.indexrelid IS NULL THEN 'no unique key, so no surveyor'
               WHEN o.unclassed IS NOT NULL AND EXISTS (SELECT 1 FROM pg_am WHERE amname = 'surveyor')
                   THEN 'its key holds ' || o.unclassed || ', a type the surveyor has no class for'
               WHEN octet_length(o.surveyor) > 63
                   THEN 'its surveyor would be named ' || o.surveyor || ', longer than 63 bytes'
               WHEN to_regclass(format('%I.%I', o.nspname, o.surveyor)) IS NOT NULL
                   THEN format('%I.%I', o.nspname, o.surveyor) || ' already exists'
           END AS why
    FROM ordered o
),
given AS (
    SELECT o.* FROM ordered o JOIN why_none w ON w.oid = o.oid WHERE w.why IS NULL
),
above (oid, ancestor) AS (
    SELECT i.inhrelid, i.inhparent FROM pg_inherits i WHERE i.inhrelid IN (SELECT oid FROM given)
  UNION
    SELECT a.oid, i.inhparent FROM above a JOIN pg_inherits i ON i.inhrelid = a.ancestor
)
    SELECT 0 AS part, 0 AS at, '' AS nspname, '' AS relname,
           'CREATE EXTENSION IF NOT EXISTS warren_surveyor_pg;' AS line
    WHERE NOT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'warren_surveyor_pg')
  UNION ALL
    SELECT 0, 1, '', '', '-- each key''s types are checked against its classes once it exists'
    WHERE NOT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'warren_surveyor_pg')
  UNION ALL
    SELECT 1, 0, '', '',
           format('ALTER DATABASE %I SET session_preload_libraries = %s;', current_database(),
                  (SELECT string_agg(quote_literal(library), ', ' ORDER BY at)
                   FROM (SELECT at, library FROM surveyor_preload
                         UNION ALL SELECT 9223372036854775807, 'warren_surveyor_pg') l))
    WHERE NOT EXISTS (SELECT 1 FROM surveyor_preload WHERE ours)
  UNION ALL
    SELECT 1, 1, '', '',
           format('-- the database preloads nothing of its own: the server''s %s stay in its list', p.libraries)
    FROM surveyor_preload_list p
    WHERE NOT p.own AND p.libraries <> '' AND NOT EXISTS (SELECT 1 FROM surveyor_preload WHERE ours)
  UNION ALL
    SELECT 2, -depth, nspname, relname,
           format('CREATE INDEX %I ON %I.%I USING surveyor (%s);', surveyor, nspname, relname, columns)
    FROM given
  UNION ALL
    SELECT 3, 0, g.nspname, g.relname, format('ANALYZE %I.%I;', g.nspname, g.relname)
    FROM given g
    WHERE NOT EXISTS (SELECT 1 FROM above a JOIN given h ON h.oid = a.ancestor WHERE a.oid = g.oid)
  UNION ALL
    SELECT 4, 0, o.nspname, o.relname, format('-- %I.%I: %s', o.nspname, o.relname, w.why)
    FROM ordered o JOIN why_none w ON w.oid = o.oid
    WHERE w.why IS NOT NULL;
\else
CREATE TEMP TABLE surveyor_plan AS
WITH surveyors AS (
    SELECT n.nspname, i.relname
    FROM pg_index x
    JOIN pg_class i ON i.oid = x.indexrelid
    JOIN pg_am a ON a.oid = i.relam
    JOIN pg_namespace n ON n.oid = i.relnamespace
    WHERE a.amname = 'surveyor' AND i.relnamespace IN (SELECT oid FROM surveyor_schemas)
      -- a partition's surveyor goes with its partitioned table's
      AND NOT EXISTS (SELECT 1 FROM pg_inherits h WHERE h.inhrelid = x.indexrelid)
)
    SELECT 1 AS part, 0 AS at, nspname, relname, format('DROP INDEX %I.%I;', nspname, relname) AS line
    FROM surveyors
  UNION ALL
    SELECT 2, 0, '', '',
           CASE WHEN EXISTS (SELECT 1 FROM surveyor_preload WHERE NOT ours)
                THEN format('ALTER DATABASE %I SET session_preload_libraries = %s;', current_database(),
                            (SELECT string_agg(quote_literal(library), ', ' ORDER BY at)
                             FROM surveyor_preload WHERE NOT ours))
                WHEN p.own THEN format('ALTER DATABASE %I RESET session_preload_libraries;', current_database())
                ELSE '-- the server''s configuration preloads warren_surveyor_pg: take it out there'
           END
    FROM surveyor_preload_list p
    WHERE EXISTS (SELECT 1 FROM surveyor_preload WHERE ours);
\endif
\set QUIET off
SELECT line FROM surveyor_plan ORDER BY part, at, nspname, relname
\g
\if :applying
\echo '-- applying'
\gexec
COMMIT;
-- the checks: each prints a row only where it fails
CREATE TEMP VIEW surveyor_checks AS
WITH surveyors AS (
    SELECT x.indrelid, x.indexrelid, x.indisvalid, x.indisready, x.indislive, i.relname, n.nspname
    FROM pg_index x
    JOIN pg_class i ON i.oid = x.indexrelid
    JOIN pg_am a ON a.oid = i.relam
    JOIN pg_namespace n ON n.oid = i.relnamespace
    WHERE a.amname = 'surveyor' AND i.relnamespace IN (SELECT oid FROM surveyor_schemas)
),
preloaded AS (
    SELECT EXISTS (SELECT 1 FROM surveyor_preload WHERE ours) AS yes,
           (SELECT libraries FROM surveyor_preload_list) AS libraries
)
SELECT 'two surveyors on one table: ' || x.indrelid::regclass::text AS failed
FROM surveyors x WHERE :'turning_on'::boolean GROUP BY x.indrelid HAVING count(*) > 1
UNION ALL
SELECT 'a surveyor not valid and ready: ' || format('%I.%I', nspname, relname)
FROM surveyors WHERE :'turning_on'::boolean AND NOT (indisvalid AND indisready AND indislive)
UNION ALL
SELECT 'a surveyor holding pages: ' || format('%I.%I', nspname, relname)
FROM surveyors WHERE :'turning_on'::boolean AND pg_relation_size(indexrelid) > 0
UNION ALL
SELECT 'no surveyor stands' WHERE :'turning_on'::boolean AND NOT EXISTS (SELECT 1 FROM surveyors)
UNION ALL
SELECT 'a table the plan gave a surveyor has none: ' || format('%I.%I', p.nspname, p.relname)
FROM surveyor_plan p
WHERE :'turning_on'::boolean AND p.part = 2
  AND NOT EXISTS (SELECT 1 FROM surveyors s
                  WHERE s.indrelid = to_regclass(format('%I.%I', p.nspname, p.relname)))
UNION ALL
SELECT 'new sessions do not preload warren_surveyor_pg: ' || libraries
FROM preloaded WHERE :'turning_on'::boolean AND NOT yes
UNION ALL
SELECT 'a surveyor still stands: ' || format('%I.%I', nspname, relname)
FROM surveyors WHERE NOT :'turning_on'::boolean
UNION ALL
SELECT 'new sessions still preload warren_surveyor_pg: ' || libraries
FROM preloaded WHERE NOT :'turning_on'::boolean AND yes;
SELECT * FROM surveyor_checks;
SELECT count(*) > 0 AS failed FROM surveyor_checks \gset
\if :failed
DO $$ BEGIN RAISE EXCEPTION 'a check failed: the lines above name each one'; END $$;
\else
SELECT format('-- every check holds; the surveyor is %s for database %s: reconnect to plan with it',
              CASE WHEN :'turning_on'::boolean THEN 'on' ELSE 'off' END, current_database());
\endif
\endif
