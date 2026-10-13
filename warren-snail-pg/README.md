# warren-snail-pg

warren-snail-pg mounts a [warren-snail](https://github.com/soulstompp/warren-snail) file in
PostgreSQL as a schema of foreign tables, read only. The file stays where it lies, one file, and
PostgreSQL reads it through the snail.

Each scan of one of its tables is a round of the snail's own, inside PostgreSQL's. While the
statement is planned, the snail prices the scan from its directories: the pages it will read, and
the fewest and the most rows its counts allow. While the statement runs, it reads exactly the
pages it predicted, a block at a time, and hands the rows up. It reads the conditions it can itself,
so the blocks they rule out are never read, and a count its counts answer reads no column page at
all. PostgreSQL filters by the rest, and joins, groups and sorts above the scans as it does above
any other.

`EXPLAIN` prints the snail's round under each scan: the order it read along, each order it priced,
and its pages, predicted and read. They are the pages the same question reads when it is asked of
the snail on its own. So a snail sent as one file is a schema mounted, its answers set beside
PostgreSQL's own tables in the same database.

It is the extension `warren_snail_pg`, and its foreign-data wrapper is `warren_snail`. It runs on
Linux, on PostgreSQL 14 to 18.

## Install

From a clone of this repository, in `warren-snail-pg/`, with what *Requirements* (under
*Reference*) lists in place:

```sh
cargo pgrx install --release --pg-config "$PG_CONFIG"
```

It builds the extension and copies its library, `warren_snail_pg.so`, into the directory
`"$PG_CONFIG" --pkglibdir` prints, and its control file and SQL into the server's extension
directory, for the server `pg_config` belongs to. Installing changes no database: the wrapper
exists only in a database that creates the extension, and no file is read until a server names it.
*Uninstalling*, under *Reference*, takes it back out.

## A first mount

Take the shop of warren-snail's first example: `shop.snail`, written from `shop.sql`, holding its
five orders. Put it where the server's own user can read it, here `/srv/snails/shop.snail`. Then, as
a superuser, in a database:

```sql
CREATE EXTENSION warren_snail_pg;
CREATE SERVER shop FOREIGN DATA WRAPPER warren_snail OPTIONS (file '/srv/snails/shop.snail');
CREATE SCHEMA shop;
SELECT warren_snail_functions('shop', 'shop') \gexec
IMPORT FOREIGN SCHEMA shop FROM SERVER shop INTO shop;
```

The server names the file. `warren_snail_functions` gives the `CREATE FUNCTION` statements the
snail's DDL declares in the schema `shop`, here `shop.clock`, and psql's `\gexec` runs each.
`IMPORT FOREIGN SCHEMA` makes one foreign table for each of the snail's tables in that schema, its
columns the snail's, in its order, of its types:

```sql
\d shop.orders
```

```text
                              Foreign table "shop.orders"
   Column    |           Type           | Collation | Nullable | Default | FDW options
-------------+--------------------------+-----------+----------+---------+-------------
 order_id    | bigint                   |           | not null |         |
 customer_id | integer                  |           | not null |         |
 placed_at   | timestamp with time zone |           | not null |         |
 total       | numeric(10,2)            |           | not null |         |
Server: shop
FDW options: ("table" 'shop.orders')
```

Ask for customer 7's orders, as the snail's first example asks, their instants on UTC's clock:

```sql
SET timezone = 'UTC';
SELECT o.order_id, o.placed_at, o.total FROM shop.orders o WHERE o.customer_id = 7;
```

```text
 order_id |       placed_at        | total
----------+------------------------+-------
        1 | 2026-01-05 10:00:00+00 | 12.50
        3 | 2026-02-01 09:30:00+00 |  8.00
(2 rows)
```

And its plan, on PostgreSQL 18:

```sql
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT o.order_id, o.placed_at, o.total FROM shop.orders o WHERE o.customer_id = 7;
```

```text
Foreign Scan on orders o (actual rows=2.00 loops=1)
  Snail File: /srv/snails/shop.snail
  Snail Query: SELECT "order_id", "placed_at", "total" FROM "shop"."orders" WHERE "customer_id" = 7
  Snail Order: orders_customer
  Snail Orders Priced: orders_customer 4 pages
  Snail Directory Pages: 2
  Snail Column Pages Predicted: 4
  Snail Column Pages Priced: 4
  Snail Rows: 0 to 5
  Snail Rows Estimated: 1
  Statistics Rows: 1
  Snail Rows Told: 1
  Snail Column Pages Read: 4
  Snail Pages Held: 0
```

The scan asked the snail the question in `Snail Query`: the three columns the statement reads, of
the rows where `customer_id = 7`. The snail read it along `orders_customer`, the one order that
holds those columns. It read 2 pages of that order's directory, predicted that the rows lie in 4
column pages, and read those 4: the round `warren-snail read` prints for the same question on its
own. The rest of this page takes those lines one node at a time.

## The scan: the snail's round

**What it reads.** The columns the statement needs of the table, and no others, along the order
the snail prices cheapest of those that hold them, of the rows the conditions it reads leave. The
round begins when PostgreSQL asks for the first row, and hands the rows up a block at a time.

**The read it cancels.** As on its own: every column the statement does not read, every order but
one, and every block the counts say holds none of the rows.

**How it shows.** `Snail Query` is the question the scan asked. `Snail Order` is the order it read
along; `Snail Orders Priced`, each order it priced, at its pages, or at its fewest to its most
pages and `left unread`. `Snail Directory Pages` and `Snail Column Pages Predicted` are the pages
it read of the directories and the column pages it predicted, before it read any; under
`ANALYZE`, `Snail Column Pages Read`, the column pages it read.

**Where it stops.** A scan reads one table. PostgreSQL joins, groups and sorts above it. Each
customer's orders and their total, from the snail's example:

```sql
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT o.customer_id, count(*) AS orders, sum(o.total) AS spent FROM shop.orders o
GROUP BY o.customer_id ORDER BY o.customer_id;
```

```text
GroupAggregate (actual rows=3.00 loops=1)
  Group Key: customer_id
  ->  Sort (actual rows=5.00 loops=1)
        Sort Key: customer_id
        Sort Method: quicksort  Memory: 25kB
        ->  Foreign Scan on orders o (actual rows=5.00 loops=1)
              Snail File: /srv/snails/shop.snail
              Snail Query: SELECT "customer_id", "total" FROM "shop"."orders"
              Snail Order: orders_customer
              Snail Orders Priced: orders_customer 2 pages; orders_month 2 pages
              Snail Directory Pages: 4
              Snail Column Pages Predicted: 2
              Snail Column Pages Priced: 2
              Snail Rows Told: 5
              Snail Column Pages Read: 2
              Snail Pages Held: 0
```

The snail read two columns of five rows; PostgreSQL sorted and grouped them. A scan begun again,
as the inner side of a nested loop is, reads a round each time, on the file as it stood when the
scan began, and `Snail Rounds` counts them. Parallel workers never read a scan: the backend that
planned it does. Inside PostgreSQL the snail holds no pages of its own (`Snail Pages Held`): every
page a round reads is read from the file, and shared buffers hold none of them, so `BUFFERS`
counts none.

## The conditions the snail reads

**What it reads.** A condition on a column, on an instant on UTC's clock (`shop.clock(placed_at)`),
or on that instant's month (`extract(month FROM shop.clock(placed_at))::smallint`), compared with
a constant by `=`, `<`, `<=`, `>` or `>=`, among the conditions `AND` joins. The snail reads these
as its own question's. PostgreSQL filters by every other condition, from the rows the scan hands
up.

**The read it cancels.** Every block those conditions rule out. Where the statement needs no
column, only the rows, the scan asks the snail for its count, and the counts answer it.
February's orders, counted:

```sql
EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF)
SELECT count(*) AS orders FROM shop.orders o
WHERE extract(month FROM shop.clock(o.placed_at))::smallint = 2;
```

```text
Aggregate (actual rows=1.00 loops=1)
  ->  Foreign Scan on orders o (actual rows=2.00 loops=1)
        Snail File: /srv/snails/shop.snail
        Snail Query: SELECT count(*) FROM "shop"."orders" WHERE extract(month FROM "placed_at" AT TIME ZONE 'UTC')::smallint = 2
        Snail Order: orders_customer
        Snail Orders Priced: orders_customer 0 pages; orders_pkey 0 pages; orders_month 1 to 1 pages, left unread
        Snail Directory Pages: 3
        Snail Column Pages Predicted: 0
        Snail Column Pages Priced: 0
        Snail Rows: 2 to 2
        Snail Rows Estimated: 2
        Statistics Rows: 1
        Snail Rows Told: 2
        Snail Column Pages Read: 0
        Snail Pages Held: 0
```

The counts said 2, and no column page was read. The scan handed PostgreSQL's `Aggregate` two rows
with no columns, and it counted them.

**How it shows.** The condition inside `Snail Query`, and no `Filter:` line for it under the scan.
A condition PostgreSQL keeps shows as the scan's `Filter:`.

**Where it stops.** The constant must be one the statement holds as it is planned, so a parameter
of a generic plan stays PostgreSQL's, as do a condition joined by `OR` and any function of a column
but the clock and its month. Text is compared by its bytes, as the imported columns' collation `C`
compares it; under another collation, only `=` is the snail's, where the collation is
deterministic. A condition on `bytea` is PostgreSQL's.

## The price, and the rows the planner is told

**What it reads.** While the statement is planned, the snail prices each scan from its directories
alone: the column pages it will read, and the fewest and the most rows the question holds, as its
counts bound them, with its own estimate between the two, from the page maps of the blocks it
reads. PostgreSQL prices each of those pages as a page read in turn: the snail's page is 4 KiB,
half of PostgreSQL's, so each costs half of `seq_page_cost`. The pages read as the snail opens (its
root and its catalog) and those of its directory come before the first row; the column pages, and
PostgreSQL's cost for each row, after.

**The read it cancels.** The scan it would cost to learn the rows: the counts give their bounds
before any column page is read. Where they meet, as for February's count above (`Snail Rows: 2 to
2`), the rows are exact.

**How it shows.** `Snail Column Pages Priced`; `Snail Rows`, the fewest to the most;
`Snail Rows Estimated`, the snail's estimate; `Statistics Rows`, PostgreSQL's own estimate of the
conditions the snail reads; and `Snail Rows Told`, the rows the planner was told, which the scan's
`rows=` shows. Customer 7's orders, planned:

```sql
EXPLAIN SELECT o.order_id, o.placed_at, o.total FROM shop.orders o WHERE o.customer_id = 7;
```

```text
Foreign Scan on orders o  (cost=2.50..4.51 rows=1 width=32)
  Snail File: /srv/snails/shop.snail
  Snail Query: SELECT "order_id", "placed_at", "total" FROM "shop"."orders" WHERE "customer_id" = 7
  Snail Order: orders_customer
  Snail Orders Priced: orders_customer 4 pages
  Snail Directory Pages: 2
  Snail Column Pages Predicted: 4
  Snail Column Pages Priced: 4
  Snail Rows: 0 to 5
  Snail Rows Estimated: 1
  Statistics Rows: 1
  Snail Rows Told: 1
```

At PostgreSQL's default costs: 3 pages as the snail opens and 2 of its directory, 2.50 before the
first row; then 4 column pages and one row, 4.51 in all. Five orders lie in one block, and the
counts stop at the place that holds about a page, so they bound customer 7's rows at 0 to 5.

**Where it stops.** The setting `warren_snail.rows` says which rows the planner is told. Unless it
is set, `mixed`: the snail's estimate, with PostgreSQL's share for a condition the snail holds no
key for (text off its order's levels, or a range on text), held between the fewest and the most.
`counts` is the snail's estimate alone; `statistics`, PostgreSQL's estimate held between the two;
`high`, the most the counts allow.

## ANALYZE

**What it reads.** `ANALYZE` of a foreign table reads every row the snail holds, and keeps a sample
of them as PostgreSQL's own sampler keeps it, for the column statistics. The table's rows and pages
are the snail's own counts.

**The read it cancels.** None: it is the one read of the whole table, made when you run it.
Autovacuum never analyzes a foreign table, so analyze one once it is mounted, and again once the
snail changes.

**How it shows.** In `Statistics Rows`, and in `pg_stats`. Customer 7's orders, analyzed:

```sql
ANALYZE shop.orders;
EXPLAIN SELECT o.order_id, o.placed_at, o.total FROM shop.orders o WHERE o.customer_id = 7;
```

```text
Foreign Scan on orders o  (cost=2.50..4.51 rows=1 width=22)
  Snail File: /srv/snails/shop.snail
  Snail Query: SELECT "order_id", "placed_at", "total" FROM "shop"."orders" WHERE "customer_id" = 7
  Snail Order: orders_customer
  Snail Orders Priced: orders_customer 4 pages
  Snail Directory Pages: 2
  Snail Column Pages Predicted: 4
  Snail Column Pages Priced: 4
  Snail Rows: 0 to 5
  Snail Rows Estimated: 1
  Statistics Rows: 2
  Snail Rows Told: 1
```

PostgreSQL's estimate is now its statistics', 2. Under `mixed` the planner is still told the
snail's estimate, since the snail holds a key for `customer_id`.

**Where it stops.** The statistics are PostgreSQL's, as for any table; the snail's counts and pages
are read afresh as each statement is planned.

## Who may mount a file

**What it reads.** The file a server names, as the server's own user, with the snail's own reads,
never through shared buffers. A name that is not absolute lies under the data directory.

**The read it cancels.** Nothing is copied in: the file is read where it lies, and only the pages a
round predicts.

**How it shows.** `Snail File`, the file as it is read.

**Where it stops.** Only a superuser, or a role with the privileges of `pg_read_server_files`, may
name the file, as PostgreSQL's own `file_fdw` has it: whoever names it reads any file the server
can. A mounted snail is read only; PostgreSQL refuses every write:

```sql
INSERT INTO shop.orders (order_id, customer_id, placed_at, total)
VALUES (6, 7, '2026-03-09 08:20:00+00', 21.00);
```

```text
ERROR:  cannot insert into foreign table "orders"
```

## Reference

### Requirements

Linux, PostgreSQL 14 to 18, Rust 1.96 or later and cargo-pgrx 0.19.2. The workspace's
[README][workspace] gives the prerequisites, and [warren-surveyor-pg's][surveyor] *Requirements*
the one line of `~/.pgrx/config.toml` that tells pgrx where your PostgreSQL is.
`CREATE EXTENSION warren_snail_pg` takes a superuser: the extension is not trusted. It builds
against [warren-snail](https://crates.io/crates/warren-snail), without its feature `standalone`.

### What it installs

- the functions `warren_snail_handler()` and `warren_snail_validator(text[], oid)`, and the
  foreign-data wrapper `warren_snail`, with a comment `\dew+` shows;
- the function `warren_snail_functions(server text, schema text)`;
- the setting `warren_snail.rows`, registered when the library loads.

### The objects and their options

- **A server** takes one option, `file`, the snail it reads.
- **A foreign table** takes one option, `table`, the snail's table it stands for, as
  `schema.table`. Its columns are named as the snail's; it may leave some out.
- **`IMPORT FOREIGN SCHEMA <schema> FROM SERVER <server> INTO <local>`** makes a foreign table for
  each of the snail's tables in `<schema>`, named as the snail names it; `LIMIT TO` and `EXCEPT`
  keep or leave tables, by those names. It takes no options. It needs `USAGE` on the server and
  `CREATE` on `<local>`.
- **`warren_snail_functions(server, schema)`** returns, one row each, the `CREATE FUNCTION`
  statements the snail's DDL declares in `schema`, as the DDL writes them. It needs `USAGE` on the
  server.

An option on another object, an option given twice, and one left out or empty are refused, each
with its reason.

### The types

| the snail's | the foreign table's, as imported | may also be declared |
|---|---|---|
| `smallint`, `integer`, `bigint` | the same | |
| `numeric(p, s)` | `numeric(p, s)` | `numeric`, or a wider `numeric` of the same scale |
| `text` | `text COLLATE "C"` | `varchar` |
| `varchar(n)` | `varchar(n) COLLATE "C"` | `text`, `varchar`, or a longer `varchar` |
| `bytea` | `bytea` | |
| `timestamptz` | `timestamp with time zone` | |

The snail's largest and smallest instants are PostgreSQL's `infinity` and `-infinity`. A column
the snail does not hold, or of a type that cannot hold every value of the snail's column, is
refused, with its reason, when a statement that reads the table is planned.

### What EXPLAIN shows

Under each scan, in this order: `Snail File`, `Snail Query`, `Snail Order`, `Snail Orders Priced`,
`Snail Directory Pages`, `Snail Column Pages Predicted`, `Snail Column Pages Priced`; where the
snail reads a condition, `Snail Rows`, `Snail Rows Estimated` and `Statistics Rows`; then
`Snail Rows Told`. Under `ANALYZE`, also `Snail Column Pages Read`, `Snail Pages Held` and,
where the scan read more than one round, `Snail Rounds`. In `FORMAT JSON` and the other formats,
each is a property of the scan's node, by the same name.

### Settings

`warren_snail.rows`: `mixed` (unless set), `counts`, `statistics` or `high`, as *The price, and the
rows the planner is told* says. Any role may set it, for a session or a transaction.

### Uninstalling

1. `DROP EXTENSION warren_snail_pg CASCADE;` drops the wrapper and its functions, every server on
   it and every foreign table on those. The functions `warren_snail_functions` gave you are yours,
   and stay. The snail's files are not touched.
2. Remove the library, the control file and the SQL from the directories *Install* copied them
   into.

## License

Copyright (C) 2026 Kenneth Allen Flegal

This program is free software: you can redistribute it and/or modify it under the terms of the GNU
General Public License as published by the Free Software Foundation, either version 3 of the
License, or (at your option) any later version. See [LICENSE][LICENSE].

[workspace]: https://github.com/soulstompp/warren-surveyor/blob/HEAD/README.md
[surveyor]: https://github.com/soulstompp/warren-surveyor/blob/HEAD/warren-surveyor-pg/README.md
[LICENSE]: https://github.com/soulstompp/warren-surveyor/blob/HEAD/warren-snail-pg/LICENSE
