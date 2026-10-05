# warren-surveyor-pg

*It measures without digging.*

The surveyor is an index for PostgreSQL 18 that is never scanned and holds nothing. You put one on
a table, on the table's own unique key. While a statement is planned, it reads the indexes you
already have, your B-trees and the GIN and GiST indexes that search words, trigrams, boxes and
distances, from the top, stopping above the leaf pages wherever it can. Then it hands the planner
what it finds:

- the rows each table's conditions select;
- the column statistics that joins and groupings are sized by;
- the rows a `WITH RECURSIVE` query returns, read by running it while the statement is planned;
- the price of reading your B-trees, from the pages of them that shared buffers hold;
- how busy the drive is, which weighs every page a plan would read from the disk.

The planner still chooses the plan, from these where it would otherwise estimate: no index is
forced, and no answer changes. Each of the surveyor's reads is there to cancel a bigger one, the
scan a plan would make on a guess. You see it in `EXPLAIN (ANALYZE, BUFFERS)`: each scan's
estimated rows beside its actual rows, the pages each scan reads from outside shared buffers, and
the surveyor's own reads under `Planning:`. Together those reads draw on one budget, the pages of
the tables the statement reads, and where they stop, PostgreSQL's own estimate stands.

The same library plans a `SELECT` through another spelling of it that returns the same rows and is
written to read less: a join every arm of a `UNION ALL` shares is made once, a grouping over a join
is taken on each side first, and one side is grouped only at the keys the other holds.

It is the extension `warren_surveyor_pg`, and its access method is `surveyor`. To watch it work on
your own server, on a database built to trip indexes, with every answer checked, start with
[the kit](../kit/README.md): one command, `kit/run.sh`, runs it all.

## Install

From a clone of this repository, in `warren-surveyor-pg/`, with what *Requirements* (under
*Reference*) lists in place:

```sh
cargo pgrx install --release --pg-config "$PG_CONFIG"
```

It builds the extension and copies its library, `warren_surveyor_pg.so`, into the directory
`"$PG_CONFIG" --pkglibdir` prints, and its control file and SQL into the server's extension
directory, for the server `pg_config` belongs to. Everything below starts from those files:
`CREATE EXTENSION` reads them, and a session that loads the library loads it from there. To see
that the server has them:

```sql
SELECT name, default_version FROM pg_available_extensions WHERE name = 'warren_surveyor_pg';
```

It gives `0.1.0`. Installing changes no database and no setting: the library acts only in a session
that loads it, and the access method exists only in a database that creates the extension.
*Uninstalling*, under *Reference*, takes it back out.

## Why the disk comes first

On a running server the disk is always busy. Every page a plan reads from it waits behind every
other query, VACUUM and the writes, and a time taken on a quiet machine cannot show that wait. So
the surveyor keeps one rule above the rest: avoid the disk. A plan that reads the disk and runs a
little faster has lost to one that reads none.

The rule holds for the surveyor too. We leave the table alone as much as we can, and justify every
read we make: each one below is given with the scan it cancels.

## A first example

With the extension installed and the catalogue loaded (the kit's step 1), take two of its tables:
themes, each under its parent theme, and the sets in each theme. The load gives each its primary
key and a B-tree for the join into it: `lego_themes` on `(parent_id, id)`, `lego_sets` on
`(theme_id, set_num)`. Ask for the sets under Castle, theme by theme, and keep the plan:

```sql
SET search_path = lego, public;

EXPLAIN (ANALYZE, BUFFERS)
WITH RECURSIVE castle (id) AS (
    SELECT t.id FROM lego_themes t WHERE t.id = 186
  UNION ALL
    SELECT t.id FROM lego_themes t JOIN castle c ON t.parent_id = c.id
)
SELECT s.theme_id, count(*) AS sets
FROM castle
JOIN lego_sets s ON s.theme_id = castle.id
GROUP BY s.theme_id
ORDER BY s.theme_id;
```

Then turn the surveyor on for the two tables:

```sql
CREATE EXTENSION warren_surveyor_pg;
-- every new session loads the library before its first statement
-- (keep in the list any library the database preloads already)
ALTER DATABASE lego SET session_preload_libraries = 'warren_surveyor_pg';
-- one surveyor on each table, on the table's own unique key
CREATE INDEX lego_themes_surveyor ON lego.lego_themes USING surveyor (id);
CREATE INDEX lego_sets_surveyor   ON lego.lego_sets   USING surveyor (set_num);
ANALYZE lego.lego_themes, lego.lego_sets;
```

`warren-bench kit surveyor-sql | psql -X -d lego -v mode=on -v apply=1 -f -` does this for every
table of a database at once, and `-v mode=off` undoes it. Reconnect, so that the new session loads
the library, and run the same `EXPLAIN`.

**What it reads.** While the statement is planned, the surveyor runs the walk down the themes once,
and reads a B-tree of the sets that leads with `theme_id` at each theme the walk returned.

**The scan it cancels.** Taken down to the parts' colours (the kit's question 19), the same
statement read 754,116 pages from outside shared buffers at the median on PostgreSQL alone, and
39,818 with the surveyor, in the kit's run at `huge`. On this one it cancels none: both plans read
every page from shared buffers, and the surveyor's plan touched 187 pages against 158.

**In `EXPLAIN`.**

- **No surveyor appears in the plan.** A surveyor is never scanned.
- **`CTE Scan on castle`** shows exactly the rows the walk returns, where PostgreSQL alone shows
  its estimate. The `Recursive Union` beneath it keeps the planner's own estimate, so the two stand
  side by side.
- **The join to the sets** is sized by the sets each theme the walk returned holds, rather than by
  one average.
- **`Planning:`** shows more pages: the walk's, and the B-tree pages the surveyor read. In the
  kit's run it touched 205 pages in 1.22 ms, where PostgreSQL alone touched none in 0.13 ms. That
  is the price; each scan's pages below it are where it is paid back, or not.

**Where it stops.** The walk may read no more pages than `lego_themes` and `lego_sets` hold, and
return no more rows than they hold; past either, the planner keeps its estimate.

## What it reads while planning

Everything here happens while a statement is planned, for the tables that have a surveyor; a
statement that reads or writes none of them is planned as PostgreSQL plans it, the respelling
apart. Every page these reads take draws on one budget for the statement: the pages of the tables
it reads, each counted once, those of its subqueries, sublinks and `WITH` queries among them, and
every member of an inheritance parent or a partitioned table read with its members; a table an
`INSERT` only writes does not count. That is the scan the reads are there to cancel, and together
they are held to it (`warren_surveyor_pg.planning_read_limit` lowers it), but for a respelling
that fails, which gives back what it drew (see *The respelling*). An index page draws one
page, found in shared buffers or read in. Where a read stops at the budget, PostgreSQL's own
estimate stands for what it would have measured. What is measured is kept while the statement is
planned, and forgotten when planning returns.

### Your B-trees: the rows a table's conditions select

**What it reads.** For each table, the B-trees whose key holds its constant conditions, each read
for the conditions no B-tree before it has counted:
equalities, lists (`IN`, `= ANY`) and `IS NULL` on the key's columns, then one range on the next; a
key column ahead of the last one they hold that no condition fixes is stepped through its values,
as a skip scan steps it. Each is read from its root down through pages above the leaves, then a few
leaves of the entries selected, or all of them where a few cannot speak for the rest; entries on
one leaf or two are counted one by one. Where `ANALYZE` read every row of the table, a leading
value its most common values name keeps `ANALYZE`'s count, and no leaf is read for it.

**The scan it cancels.** The planner sizes a table's rows from `ANALYZE`'s sample. A count off by
far chooses the wrong way in: a sequential scan of the whole table where an index would read a few
pages, or a loop run many thousands of times where one scan would do.

**In `EXPLAIN`.** The table's scan shows the measured count as its estimated rows. A parallel scan
shows it divided among its workers, as the planner divides it; a lookup on the inner side of a loop
shows its own estimate scaled with it, never above the table's measured count, and at most one
row where its equalities fix every column of a unique key. On a 46 GB database, December 2020's
purchases were 886,349 rows: the surveyor, reading 14 pages of a B-tree while planning, put them
at 881,029; PostgreSQL alone at 41,870.

**Where it stops.** Only constants are measured: a value the planner can reduce to a constant is;
a parameter of a generic plan (`plan_cache_mode` decides which plan a prepared statement gets), or
a column of another table, keeps the planner's estimate. A condition compared under another
collation than its key column keeps is not held, as PostgreSQL will not scan the B-tree for it, and
an equality under another collation, one that ignores case among them, holds no lookup to one row.
A read stepping through a key column is sized before it starts, and does not start where it would
pass the budget; any other read stops at the page that would. A partial B-tree is not read, and
neither is a table read with `TABLESAMPLE`.

### GIN: words and trigrams

**What it reads.** For a text search (`@@`) on a `tsvector` GIN, each word's entry: the length of
the list of rows beside it, or, where a word's rows fill a tree of their own, that tree's root and
a few of its leaves. The words are combined as PostgreSQL combines them for `@@`. For a trigram GIN
(`pg_trgm`), an `ILIKE`, and a `LIKE` whose pattern holds no letter that changes with case: the
fewest rows any trigram the pattern requires holds.

**The scan it cancels.** The scan of the table planned on a sample's idea of how common a word or
a pattern is.

**In `EXPLAIN`.** The table's scan shows the measured rows. On a 46 GB database, the sets whose
name contains "aeroflot" were 76 rows: the surveyor, reading 33 pages of a trigram GIN, put them at
76; PostgreSQL alone at 192.

**Where it stops.** A GIN of one column over the whole table is read; a partial one, or one of
more columns, is not. A word with a weight or a prefix (`torso:*`) is left to the planner, and so
are a `LIKE` whose pattern holds a letter that changes with case and a condition compared under
another collation than the key's own. The rows waiting in the index's pending list are never read:
the share the rest of the index gives is taken for them too.

### GiST: boxes and distances

**What it reads.** For a GiST of cubes (`cube`, and `earthdistance`'s earths): a box, the indexed
value inside or overlapping a constant box (`earth_box(…) @> ll_to_earth(…)`), and a distance, a
bound `<` or `<=` on `earth_distance` from a constant point, or on `cube_distance` or `<->` from
one. A box beside its distance is counted once with it. The read follows the index's union keys
from the root, level by level as a scan of the index does, down to the first level that names at
least twenty pages reaching the question, then reads the leaves it needs to count the rows inside.

**The scan it cancels.** The scan of the table planned without knowing whether a box holds ten
homes or ten thousand.

**In `EXPLAIN`.** The table's scan shows the measured rows. On a 46 GB database, the builders
within 50 km of London were 74,165 rows: the surveyor, reading 825 pages of the GiST, put them at
74,165; PostgreSQL alone at 671, the number it gives every radius.

**Where it stops.** A GiST of one column over the whole table is read; a partial one, or one of
more columns, is not. A distance is read only on a GiST of earths. A box or a distance compared
under another collation than the key's own is left to the planner.

### Several indexes on one table

**What it reads.** Where two or more of a table's B-trees and GIN indexes hold its conditions, the
rows in all of them are counted together:

- where a B-tree stores the value each of another index's conditions compares, a column in its key
  or its `INCLUDE`, or an expression as one of its key columns, those conditions are tested on the
  B-tree's own entries for its own conditions, and no table page is read;
- otherwise the indexes are matched by the addresses of the rows each holds: the index holding the
  fewest rows first, each next one keeping only the addresses already found. A B-tree's addresses
  come from its own scan, which reads no table page; a trigram index's may include rows its check
  against the table would drop.

**The scan it cancels.** The planner takes each condition's share alone and combines them as if
the columns had nothing to do with each other. Two conditions that pick the same rows, or rows that
never overlap, are planned far from their size, and the plan made on that guess scans.

**In `EXPLAIN`.** The table's scan shows the rows that pass every condition at once.

**Where it stops.** The addresses are held within `work_mem`, eight bytes to an address. A match
that would pass it, or the budget, stops there: the indexes it read before, where there are two or
more, are counted together, and the rest one by one. A GiST is counted on its own.

### Column statistics

**What it reads.** For a table with a surveyor over the whole table, and a column a B-tree over
the whole table leads with: the B-tree's entries holding `NULL`; and, for an integer or a date
column, its first and last values and, where the values from the first to the last are no more
than `default_statistics_target`, each value's entries. They stand in for `ANALYZE`'s share of
`NULL`s and, for an integer or a date column, its distinct values (every value from the first to
the last) and its most common values; the histogram and the order against the table stay
`ANALYZE`'s.

PostgreSQL keeps no statistics for a column of a subquery or a `WITH` query that groups by it among
others; where that column is an integer or date column passed up unchanged from a table with a
surveyor and a B-tree the column leads, or from a table `ANALYZE` read whole, it is given that
column's count of distinct values, and passed up from a table read with its members, none. A table
read with its inheritance children or its partitions is given its members' counts added together,
pruned members left out.

**The scan it cancels.** A join or a grouping planned at the wrong size hashes too much and spills
to temp, or scans a table it could have looked rows up in.

**In `EXPLAIN`.** Each join and grouping above the scans shows its estimated rows sized from these
statistics.

**Where it stops.** These reads draw at most half of the budget, half of the lowered budget where
the setting lowers it: the planner asks for a column's statistics before the surveyor measures the
table's conditions, and that read and every other one keep the rest, and may use whatever of the
whole is left. Where `ANALYZE` read every row and named every value, its statistics stand and the
B-tree is not read. Where a member of a hierarchy holding rows has no surveyor, no B-tree its
column leads, or cannot be read within the half, the hierarchy keeps `ANALYZE`'s count of distinct
values and its most common values for that column.
A column passed up through a security-barrier subquery, a set operation or grouping sets is given
no count.

### The size of a `WITH RECURSIVE` query

**What it reads.** Whatever the recursive query reads, its tables included: the query is run while
the statement is planned, and the statement is planned at the number of rows it returned. It
is run where it reads a table with a surveyor and takes nothing from the statement around it (no
parameter, no column of an outer query, no other `WITH` query, no volatile function): inside a
subtransaction, never in parallel mode, a row at a time, each time the statement is planned, and
once for each place the statement reads it.

For an integer or date column the statement equates with another table's column, the run also
keeps each value it returned with its rows, while they are no more than a statistics list holds
(10,000). Those values become the column's statistics, and the other table's column, where that
table has a surveyor and a B-tree leads with the column, is measured at them.

**The scan it cancels.** The planner estimates a recursive query from statistics that cannot see
how deep a tree runs, and sizes the join to it by one average. A join planned at the wrong size can
scan the table it joins, or hash too many rows and spill them to temp.

**In `EXPLAIN`.** Each place the statement reads the query is planned at the rows it returned, and,
where the run kept a column's values, each join on that column at the rows each value finds; the
values themselves are never written into the plan. A plain `EXPLAIN` plans the statement, so it
runs the query too; under `EXPLAIN (BUFFERS)` the query's pages, its tables' among them, are under
`Planning:`.

**Where it stops.** The run stops once its pages pass what is left of the budget, or once it has
returned more rows than the planner takes the statement's tables to hold. A run that raises an
error, a table the user may not read among them, is rolled back inside its subtransaction. Past
any of them the planner's estimate stands, and the pages the run read are still drawn on the
budget. A cancel, `statement_timeout` among them, cancels the statement. Key the tables a walk reads
by the column it joins on (for a tree, `(parent_id, id)`), so that each step of the walk is a
lookup and the run stays small.

### Shared buffers and the order of the leaves

**What it reads.** Once for each statement planned that prices a B-tree of a table with a
surveyor, one pass over the headers of shared buffers, as `pg_buffercache` reads them: which pages
of each index are in memory. No page is read. And, where
`warren_surveyor_pg.walked_leaf_page_cost` is not set, for a B-tree whose leaves a scan would walk
from outside shared buffers, the downlinks of one page above the leaves, reached from the root
by the middle downlink of each page: whether leaves next to each other in key order lie next to each
other on the disk.

**The scan it cancels.** PostgreSQL prices every index page as if read from the disk, and each as a
random read. A B-tree already in memory can then look dearer than a sequential scan of the table,
and the planner picks the scan, which reads the disk.

**In `EXPLAIN`.** A scan through any of the table's B-trees, a partial one too, shows the
surveyor's price in its `cost=`, made for that plan as the B-tree makes its own, but for its pages
and its share:

- the share a path's conditions select is the measured one, where the index holds them all;
- a page in shared buffers when the plan is made costs 0.047, whatever the tablespace's costs;
- every other page a scan reads first is priced as the disk: the page its descent reaches at the
  tablespace's `random_page_cost`; the leaves it walks after it at
  `warren_surveyor_pg.walked_leaf_page_cost` where that is set, else at the tablespace's
  `seq_page_cost` where they follow one another on the disk and `random_page_cost` where they do
  not;
- repeated scans (the inner side of a loop) count their pages fetched again with the index's own
  pages, up to the whole of `shared_buffers`, as the cache they stay in, at `random_page_cost`.

**Where it stops.** The pass over the headers draws nothing on the budget. The read of the leaves'
order draws on it; where the budget leaves no page for it, the leaves are priced as following one
another.

### The drive

**What it reads.** For the database's own tablespace and the tablespace of each table with a
surveyor that the statement reads or writes, through a view or as a member of a table it reads with
its members: the drive holding it, and from `/proc/diskstats` and `/proc/uptime`, the share of the
machine's uptime the drive has spent doing I/O. Once for each statement planned.

**The scan it cancels.** A page from a busy drive waits. Priced as if the drive were idle, a plan
that reads the disk wins on paper and loses on the server.

**In `EXPLAIN`.** Every `cost=` in the plan prices a page from the disk at `seq_page_cost`,
`random_page_cost` and `warren_surveyor_pg.walked_leaf_page_cost` (where it is set) multiplied by
the weight of the busiest of those drives: a drive busy half the time doubles the price of a page
from the disk, and one busy nine tenths of the time makes it ten times. A page in shared buffers
keeps its price, and a tablespace that declares its own page costs keeps them unweighed. The
settings are put back when planning ends, an error included.

**Where it stops.** The weight stops at a hundred. The drive is read on Linux only, and outside
parallel operations; elsewhere, and where the drive cannot be found, it counts as idle. Its share
is over the machine's whole uptime, not this minute.

### Grouping one value at a time

**What it reads.** The statement and the catalog: whether every table a `GROUP BY` reads lies
within what one value reaches, a parameter or a column of an outer row (as in a `LATERAL` over the
values), and some table it reads has a surveyor. A table with a surveyor lies within reach when one
of its B-trees leads with a column equated with the value or joined to a table already within it;
any table does when joined by its own one-column unique key to one already within it, or equated
with the value on it.

**The scan it cancels.** A sort of every row the joins make, which spills to temp once it passes
`work_mem`: a hash holds an entry for each group, where a sort holds every row.

**In `EXPLAIN`.** A `HashAggregate` makes the groups, and no `Sort` beneath a `GroupAggregate`
orders the rows for it. A grouping that reads its rows already in order is kept.

**Where it stops.** A value equated with a constant, `enable_hashagg = off`, grouping sets, a
partitionwise grouping, and aggregates that cannot be hashed leave the plan as the planner chose
it.

### The respelling

**What it reads.** In a session that has loaded the library, every `SELECT`, with a surveyor on its
tables or none, and the catalog where a rule needs it, as [warren-pg-speller's
README](../warren-pg-speller/README.md#what-it-reads-from-the-catalog) sets out. Wherever the
statement and the catalog show it returns the same rows, the statement is planned through another
spelling of it:

- *a join every arm of a `UNION ALL` shares is made once*, to a `UNION ALL` of the rest of each
  arm;
- *a grouping over a join is taken on each side first*, where its tables split into two sides
  joined by one key: side A, holding the column a `count(DISTINCT …)` counts, or where there is
  none the first aggregate of a column, and side B, the rest;
- *side B is grouped only at the keys side A holds*.

A respelled statement reads no table the statement does not, each as the statement reads it, under
the same row security.

**The scan it cancels.** As written, a `UNION ALL` reads the shared relation once for each arm, and
a join makes a row for every pair of rows sharing the key, all of which the grouping holds.
Respelled, the shared relation is read once, each side is held only as its own groups, and side
B's rows at keys side A does not hold are dropped before B is grouped.

**In `EXPLAIN`.** A respelled grouping shows side A's grouping as a `WITH` query named `side_a`,
read twice: by the join of the two sides, and under side B's grouping, which keeps only the keys
`side_a` holds.

**Where it stops.** Where any condition a rule checks fails, the statement is planned as written;
every condition is in [warren-pg-speller's README](../warren-pg-speller/README.md). Nothing is
respelled inside a parallel operation. A respelling that raises an error once it has begun to build
a spelling, a cancel apart, is rolled back, and the statement is planned as written, with the
budget and the surveyor's measures as they stood before the respelling began; the two plannings
together may then read up to twice the budget. An error while it is still deciding whether a rule
applies fails the statement.

## Checking it

Run the statement under `EXPLAIN (ANALYZE, BUFFERS)` (PostgreSQL 18 turns `BUFFERS` on with
`ANALYZE`; writing it keeps the habit), with the surveyor and without, and read in this order:

1. **The disk.** Each scan's `Buffers:` line: `shared read` is pages from outside shared buffers.
   A node's buffers include its children's, so read each scan's own.
2. **Temp.** `temp read` and `temp written`, a `Sort Method: external merge`, a `Hash` with more
   than one batch, a `HashAggregate` with `Disk Usage`: pages written to the disk and read back.
3. **Estimated against actual rows.** Each scan's `rows=` beside its `actual rows=` (per loop, on
   the inner side of a loop). Where the surveyor measured, they lie close; where they do not, the
   notes below say what it measured, and from what.
4. **Planning.** The `Planning:` section's `Buffers:` counts every page touched while planning, the
   catalog's and the surveyor's reads. `Planning Time` is its time.
5. **Then time.** `Execution Time`, last.

To see what was measured, and from what, set `client_min_messages = debug1` (or
`log_min_messages`, for the server's log) and plan the statement. The library notes:

- for each table it measured, `surveyor: <table> measured at <rows> rows of <rows> by <indexes>
  (pages read <n>); the planner had <rows>`, naming any indexes counted together, and how;
- where conditions were matched by their rows' addresses, `surveyor: <table> read by its rows'
  addresses, no index relating its conditions: …`, with each index's pages and whether the match
  stopped. That is two columns asked for together with no index relating them: a B-tree that
  stores the other column in its `INCLUDE` lets its own entries answer both;
- for each GiST it read, `surveyor: <index> read <n> pages for <k> condition(s), …`;
- where the budget stopped a read or kept it from starting, a note naming the index and its table,
  or `the WITH query <name>`, with `read stopped at the planning-read limit` or `not started at the
  planning-read limit`, `half the planning-read limit` for a statistics read;
- where a respelling failed and the statement was planned as written,
  `warren-pg-speller: planned as written, the respelling failed: <message>`.

The kit does this for 25 questions on PostgreSQL alone, on the respelling alone and with the
surveyor, the DBA's indexes the same on each, every answer checked by its row count and a digest of
its rows.

## What it costs

### Having one

Almost nothing, and nothing on the disk:

- **No pages.** Building a surveyor reads no table and writes no page: it adds its rows to the
  catalog. Built `CONCURRENTLY`, PostgreSQL's own check of a new index still reads the table once.
  It stays at 0 bytes through inserts, updates and deletes, `VACUUM`, `VACUUM FULL`, `REINDEX`
  (`CONCURRENTLY` too) and `TRUNCATE`.
- **Nothing written.** A new row is nothing to a surveyor, and writes nothing to the WAL for it.
- **HOT stays HOT.** A surveyor never keeps an update from being HOT: an update may change the
  columns it names and still be a HOT update.
- **The table's count is left alone.** A build reports no count of rows, so the table's own count
  stays as `ANALYZE` or `VACUUM` left it.
- **It is never in a plan.** No condition matches it and no plan reads it, so
  `pg_stat_user_indexes` shows its `idx_scan` at 0. A search for unused indexes finds it: it is
  used while planning, never while running.

Creating and dropping one take the locks `CREATE INDEX` and `DROP INDEX` take, until their
transaction ends: creating holds off the table's writes, and dropping waits for every query on the
table. A build without `CONCURRENTLY` is over at once.

### Planning

Every `SELECT` in a session with the library loaded pays for the respelling's look at it, and at
the catalog where a rule needs the catalog. Every statement that reads a table with a surveyor also
pays for its reads while it is planned: index pages within the budget, a recursive query's run
where there is one, the pass over shared buffers' headers, which grows with `shared_buffers`
(16,384 headers at 128 MB), and the system files the drive is read from. A statement planned once
and run many times (a prepared statement with a generic plan) pays once.

In the kit's run at `huge`, on PostgreSQL's default settings, planning took 0.45 to 57.06 ms with
the surveyor where PostgreSQL alone took 0.03 to 2.11, and touched up to 7,435 pages against up to
134, the longest where a theme tree is walked while the statement is planned; at `small`, 0.53 to
2.46 ms against 0.03 to 0.52. On a 46 GB database, of 35 single-table questions planned there for
the first time, none read more than 519 pages from outside shared buffers while planning. Where a
plan changes little, the extra planning is most of what the surveyor costs; read the kit's table
question by question, pages first.

## Reference

### Requirements

PostgreSQL 18, Rust 1.96 or later and cargo-pgrx 0.19.2. On a server installed by a package manager,
also its development package and libclang, which pgrx builds against; there `cargo pgrx install`
takes `--sudo`, since the server's directories are the system's. It builds against
`warren-pg-speller`, beside it in this repository. pgrx reads where your PostgreSQL is from
`~/.pgrx/config.toml` (or `$PGRX_HOME/config.toml`); the second line below writes that file, and
where it exists already, add the `pg18` line under its `[configs]` instead:

```sh
PG_CONFIG=/path/to/pg_config
mkdir -p ~/.pgrx && printf '[configs]\npg18 = "%s"\n' "$PG_CONFIG" > ~/.pgrx/config.toml
```

`CREATE EXTENSION warren_surveyor_pg` takes a superuser: the extension is not trusted.

### Loading the library

The surveyor and the respelling act only in a session that loaded the library before the
statement was planned:

- **`session_preload_libraries`**, set on the database or a role (`ALTER DATABASE … SET`,
  `ALTER ROLE … SET`): every new session loads it before its first statement. This is what the kit
  sets. Setting it takes a superuser, or a role granted `SET` on it; sessions already open keep
  what they started with, so reconnect.
- **`shared_preload_libraries`**: every session on the server, after a restart.
- **`LOAD 'warren_surveyor_pg'`**, as a superuser: one session, from its next statement.

Preload it. Without a preload, a session loads the library the first time it opens a surveyor,
partway through planning that statement: that statement is planned with part of the surveyor and
without the respelling, and the statements after it with all of it. A session that never opens one
never loads it. Two sessions then plan the same statement differently.

Install the library on every server the database is served from, its replicas included: a
surveyor's catalog rows and the database's preload setting replicate as any catalog rows do, and a
session cannot start where a library it must preload is missing.

### Creating a surveyor

```sql
CREATE INDEX lego_sets_surveyor ON lego.lego_sets USING surveyor (set_num);
```

- **The key.** A surveyor stands on the table's own unique key: the primary key, else the unique
  key with the fewest columns, as the kit chooses. It reads the same indexes whatever columns it
  names, and several surveyors on one table count as one, so the key names the table and is no
  tuning choice. A key column can be an expression.
- **Key types.** One operator class for each of 17 types, each ordered as the B-tree orders it:
  `int2`, `int4`, `int8`, `numeric`, `float4`, `float8`, `text`, `bpchar`, `bytea`, `bool`, `uuid`,
  `oid`, `date`, `time`, `timestamp`, `timestamptz` and `interval`. A `varchar` key takes the
  `text` class. A table whose key holds any other type gets no surveyor from the kit, which names
  it.
- **Refused**, each with the error PostgreSQL gives any index that cannot take it:

  | refused | error |
  |---|---|
  | a key of a type with no class | `data type … has no default operator class for access method "surveyor"` |
  | `ASC` or `DESC` | `access method "surveyor" does not support ASC/DESC options` |
  | `NULLS FIRST` or `NULLS LAST` | `access method "surveyor" does not support NULLS FIRST/LAST options` |
  | `INCLUDE` | `access method "surveyor" does not support included columns` |
  | `UNIQUE` | `access method "surveyor" does not support unique indexes` |
  | a storage parameter, in `WITH (…)` or `ALTER INDEX … SET` | `unrecognized parameter "…"` |
  | `CLUSTER` on it | `cannot cluster on index "…" because access method does not support clustering` |

- **Partial surveyors.** `CREATE INDEX … USING surveyor (…) WHERE …` turns the surveyor on for a
  statement only where the planner proves the `WHERE` from the statement's own conditions;
  elsewhere the table is planned as if it had none, but for the drive's weight. It never turns on
  the column statistics: they are asked for without the statement's conditions, so nothing proves
  its `WHERE`.
- **Partitions.** A surveyor on a partitioned table puts one on each partition, as any index does
  (the kit makes the partitions' first, and the partitioned table's takes them as its own). Each
  partition is measured as itself.
- **Inheritance.** A parent read with its children is measured as the sum of its children's rows.
  Give every member that holds rows a surveyor: a member holding rows without one leaves the
  hierarchy's statistics to `ANALYZE`, and its grouping to the planner. A parent holding no rows of
  its own needs none.
- **Unlogged tables** take one like any other table.

### Settings

- **`warren_surveyor_pg.walked_leaf_page_cost`** (floating point, default `-1`, any user): the
  price of a B-tree leaf read from the disk by a scan walking the leaves in key order, every step of
  the walk counted in it. Set it to the price measured on your drive; `-1` takes the tablespace's
  `seq_page_cost` for leaves that follow one another on the disk and `random_page_cost` for the
  rest.
- **`warren_surveyor_pg.planning_read_limit`** (integer, in pages, so `64MB` works; default `-1`;
  superuser only): lowers the budget to this many pages where the statement's tables hold more.
  `-1` sets no lower limit; `0` reads no index page and runs no recursive query while planning, so
  nothing is measured, while the drive's weight, the price of B-trees from shared buffers and
  grouping one value at a time still apply.

Once the library is loaded, every other name under `warren_surveyor_pg.` is refused.

Declare a drive's measured page costs on its tablespace,
`ALTER TABLESPACE … SET (seq_page_cost = …, random_page_cost = …)`, not for the whole server:
PostgreSQL prices a tablespace's pages by its declared costs, and every spill (a sort's runs, a
hash's batches) by the server's, and a spill's temp file is often deleted before the drive ever
writes it.

### Failures and logging

- A scan of a surveyor is refused, `a surveyor holds no entries and is never scanned`; no plan
  chooses one.
- `amvalidate` reports a hand-written class without a comparison function as not valid, with the
  note `surveyor operator class for <type> has no comparison function`.
- An error that fails a statement goes to the server's log as any error does. Nothing else does at
  the default levels: the notes are at `DEBUG1`, an error the respelling recovers from among them,
  and an error the recursive query's run recovers from is not logged at all. As the library loads,
  a setting under `warren_surveyor_pg.` that names nothing is removed with a `WARNING`, as
  PostgreSQL does under a prefix an extension reserves.

### Interactions

- **The plan cache.** A plan kept and run again (a prepared statement, a PL/pgSQL function) keeps
  the measures it was made with; a recursive query's rows are read again when it runs, so it
  returns what the tables hold then, planned at the size read when the plan was made. A respelled
  plan is made again when an index on a table it reads changes. Grouping one value at a time
  applies to generic plans too: the value is the parameter.
- **Other extensions' hooks.** The library puts its planner hooks in place after any already
  there, and calls each: `planner_hook`, `set_rel_pathlist_hook`, `get_relation_info_hook`,
  `get_relation_stats_hook`, `get_index_stats_hook` and `create_upper_paths_hook`. A planning that
  does not enter through `planner_hook`, such as another extension's direct call of
  `standard_planner`, keeps a budget of its own the same way.
- **Locks.** Planning waits for no lock the statement does not take itself: the tables the budget
  counts and those whose drives are read are looked up in the catalog without locking them, and a
  partition the planner prunes is never locked.
- **`pg_dump` and restore.** A surveyor is dumped as its `CREATE INDEX … USING surveyor`, and
  restored without reading its table; the extension must be installed on the server restored to.
  The database's `session_preload_libraries` travels as any database setting does.
- **Security.** Measured statistics that hold a table's values are offered to the planner only
  where the user may read every row of the column, as PostgreSQL decides for its own statistics,
  row security and column privileges included. Counts alone, which hold no value, always are, and
  so are a recursive query's values, which its run took as the user.

### What it installs

- the function `surveyor_handler(internal)`, the access method's handler, and the index access
  method `surveyor`, with a comment `\dA+` shows;
- 17 default operator classes for `surveyor`, each named for its type (`int4_ops`, `text_ops`, …),
  each with the B-tree's comparison function for its type and, for most, its sort support;
- the domain `warren_key`, `text` under the collation `pg_c_utf8`: text that orders by code point
  whatever the operating system's locale data. Where the schema has a `warren_key` already, it is
  kept only where another extension made it as that same domain and a superuser owns it, since
  whoever owns a type may attach a check to it; any other stops the install with `type
  <schema>.warren_key exists, and is not a domain over text with collation pg_c_utf8 that an
  extension made and a superuser owns`;
- the settings `warren_surveyor_pg.walked_leaf_page_cost` and
  `warren_surveyor_pg.planning_read_limit`, registered when the library loads.

It creates no table, view or function for users to call.

### Upgrading

This is version 0.1.0, the first, with no script for `ALTER EXTENSION … UPDATE`. A surveyor holds
no page, so nothing on disk moves between versions. A new library goes in with
`cargo pgrx install`; a session loads it once, so reconnect (and restart, where it is in
`shared_preload_libraries`). Where a release changes the extension's SQL, turn the surveyor off,
drop and create the extension, and turn it on: nothing in that reads a table but `ANALYZE`. A
column of type `warren_key` keeps the extension from being dropped; change it to `text` first.

### Uninstalling

1. Drop every surveyor.
   `warren-bench kit surveyor-sql | psql -X -d <database> -v mode=off -v apply=1 -f -` drops them
   all (a partitioned table's takes its partitions' with it) and takes the library out of the
   database's preload, keeping every other library there. Where a role's settings or the server's
   configuration preload it, take it out there by hand. Reconnect.
2. `DROP EXTENSION warren_surveyor_pg;` removes the access method, its operator classes, its
   handler, and the `warren_key` domain where the extension made it. It refuses while a surveyor or
   a column of type `warren_key` depends on it.
3. Remove the library, the control file and the SQL from the directories *Install* copied them
   into, only once no preload names it.

A session that still loads the library keeps respelling its statements after the extension is
dropped; only the surveyor's reads need the access method.

## License

Copyright (C) 2026 Kenneth Allen Flegal

This program is free software: you can redistribute it and/or modify it under the terms of the GNU
General Public License as published by the Free Software Foundation, either version 3 of the
License, or (at your option) any later version. See [LICENSE](LICENSE).
