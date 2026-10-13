# Changelog

## 0.1.0

The first release: a [warren-snail](https://github.com/soulstompp/warren-snail) file mounted in
PostgreSQL 14 to 18 as a schema of foreign tables, read only, on Linux.

- The foreign-data wrapper `warren_snail`: a server names the file, a foreign table the snail's
  table it stands for. Only a superuser, or a role with the privileges of `pg_read_server_files`,
  may name the file.
- `IMPORT FOREIGN SCHEMA` makes a foreign table for each of the snail's tables in a schema, of the
  snail's own types; `warren_snail_functions` gives the functions its DDL declares.
- Each scan is a round of the snail's own: priced from its directories while the statement is
  planned, its pages read as predicted, a block at a time.
- The conditions the snail reads (a column, its instant on UTC's clock or that instant's month,
  compared with a constant) handed to it; a count its counts answer reads no column page.
- `EXPLAIN` prints the round under each scan.
- `ANALYZE` by PostgreSQL's own sample of every row the snail holds.
- The setting `warren_snail.rows`, the rows the planner is told: `mixed`, the default, `counts`,
  `statistics` or `high`.
