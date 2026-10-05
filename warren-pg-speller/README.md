# warren-pg-speller

Some queries are slow only because of how they are written. Join purchases to the lines of the sets
they bought and count afterwards, and PostgreSQL builds every pair of a purchase and a line before
it can count anything. Count the purchases of each set, and the lines of each set, and join the two
counts, and the answer is the same while the join pairs sets instead of rows.

warren-pg-speller makes that second spelling for you, inside the planner: a query is planned through
another spelling of it that returns the same rows. Each of its three rules is written out below as
what it changes, the read it cancels, how it shows in `EXPLAIN` and when it applies, so that a
database engine can take any of them into its own planner. It is a Rust library for
[pgrx](https://github.com/pgcentralfoundation/pgrx) extensions: an extension calls one function from
`_PG_init`, and PostgreSQL's `planner_hook` does the rest. `warren_surveyor_pg`, beside it in this
repository, is one such extension.

To see it, run a statement under `EXPLAIN (ANALYZE, BUFFERS)`. A grouping respelled shows a WITH
query named `side_a` (`side_a_2`, … where the query has one of that name already), one side's
grouping, read twice: by the join of the two sides, and by the
other side's grouping, which keeps only the keys `side_a` holds. A UNION ALL respelled scans the
relation its arms share once. PostgreSQL alone, in a session that has not loaded the extension's
library, shows the plan as written: compare the pages each plan touches, and its temp pages.

It acts only while a SELECT is planned, and only on a query of it where every condition of a rule
holds; every other query is planned as written, and so is a statement whose spelling raises an ERROR
other than a cancel. A spelling is built from the statement as PostgreSQL parsed it, never from its
text, and reads no table the statement does not read: each as the statement reads it, as the owner
of the view it lies in or as the user running the statement, under the same row security.

## Rule 1: a join every arm of a UNION ALL shares is made once

### What it changes

A UNION ALL whose arms each join the same relation A, in the same way, to the rest of the arm
becomes A joined once to a UNION ALL of the rest of each arm.

```sql
-- as written: the purchases are read by each arm
SELECT bt.id, i.id AS inventory_id FROM bought bt JOIN inventories i ON i.set_num = bt.set_num
UNION ALL
SELECT bt.id, n.inventory_id       FROM bought bt JOIN nested n      ON n.set_num = bt.set_num

-- respelled: the purchases are read once
SELECT bt.id, arms.inventory_id
FROM bought bt
JOIN (SELECT i.set_num AS key1, i.id AS inventory_id FROM inventories i
      UNION ALL
      SELECT n.set_num, n.inventory_id FROM nested n) arms ON arms.key1 = bt.set_num
```

The new UNION ALL's arms return the rest's side of each equality with A (`key1`, `key2`, …), then
each column that does not read A. The outer query reads A and that union, `arms`, under A's own
conditions and the equalities with A's side put back, and returns A's columns and the union's in the
original order. The union's WITH queries, ORDER BY, LIMIT and OFFSET stay on the outer query.

### The read it cancels

As written, A is read once for each arm and joined in each. Respelled, it is read and joined once,
to the rows of every arm's rest together. Where A is a WITH query, as written each arm reads it;
respelled, it is read once, and where nothing else reads it PostgreSQL may read it in place instead
of storing it.

### In EXPLAIN

A's scan, or its plan, appears once, joined to an Append of the arms' rest; as written, each arm
under the Append reads A. A WITH query read in place shows no `CTE Scan`, where as written, stored
for the arms, it shows one for each arm.

### When it applies

It applies where every condition below holds, each read from the parsed query and the catalog.

1. The query's set operations are all UNION ALL, with at least two arms, and it locks no rows.
2. Every arm is a plain SELECT that reads no column or aggregate of a query around it and returns no
   hidden column, and every FROM item of it is one the rules read.
3. A, a FROM item of the first arm, is matched in every arm, the first included, by exactly one FROM
   item that reads the same rows:
   - the same table, with ONLY in both arms or in neither, and no TABLESAMPLE; the same WITH query;
     or an equal subquery, of the same view or of none, with the same security barrier;
   - read as the same role, needing the same permissions;
   - under equal row security conditions.
4. Every arm has at least one FROM item besides A.
5. Every arm has the same conditions on A alone, in the same order.
6. Every condition that reads A and the rest of its arm is an equality between an expression of A
   alone and an expression of the rest alone. Every arm has the same such equalities in the same
   order: the same expression of A, on the same side, by the same operator and collation, and the
   rest's side of the same type, type modifier and collation.
7. Every column the arms return either reads A alone in every arm, as the same expression and of the
   type and type modifier the union returns, or reads no column of A in any arm.

A is the first FROM item of the first arm for which every condition holds; where none does, the
query is planned as written. *A plain SELECT*, *a FROM item the rules read* and *an equality* are
set out in [The queries it reads](#the-queries-it-reads).

## Rule 2: a grouping over a join is taken on each side first

### What it changes

A grouped query whose FROM items split into two sides, joined only by the equalities of one set of
equal columns, the key, is grouped on each side first: each side by its own group keys and the key,
and the two groupings are joined at the key. With the primary keys `purchases (id)`,
`holdings (builder_id, row_no)` and `builders (builder_id)`, and `lines.qty` an `int`:

```sql
-- as written: every purchase is paired with every line of its set before anything is counted
SELECT b.zone, l.part, count(DISTINCT p.id) AS purchases, sum(l.qty) AS pieces
FROM purchases p
JOIN holdings h ON h.builder_id = p.builder_id AND h.row_no = p.row_no
JOIN builders b ON b.builder_id = h.builder_id
JOIN lines l    ON l.set_num = h.set_num
GROUP BY 1, 2

-- respelled: the purchases counted by zone and set, the lines summed by part and set
WITH side_a AS (
  SELECT b.zone AS group1, h.set_num AS key, count(*) AS rows
  FROM purchases p
  JOIN holdings h ON h.builder_id = p.builder_id AND h.row_no = p.row_no
  JOIN builders b ON b.builder_id = h.builder_id
  GROUP BY 1, 2)
SELECT side_a.group1 AS zone, side_b.group2 AS part,
       sum(side_a.rows::numeric)::bigint AS purchases,
       sum(side_b.side2::numeric * side_a.rows::numeric)::bigint AS pieces
FROM side_a
JOIN (SELECT l.part AS group2, l.set_num AS key, sum(l.qty) AS side2
      FROM lines l
      WHERE l.set_num IN (SELECT key FROM side_a)  -- rule 3
      GROUP BY 1, 2) side_b ON side_a.key = side_b.key
GROUP BY 1, 2
```

Side A, the purchases' side here, becomes the WITH query `side_a` (`side_a_2`, `side_a_3`, … where
the query already has a WITH query of that name), and side B the subquery `side_b`. Each is a query
over its own FROM items and conditions, grouped by its group keys (`group1`, `group2`, … by their
place in GROUP BY) and its column of the key (`key`). It returns those, the count of its rows
(`rows`) where the table below reads it, and each aggregate on its side (`side1`, `side2`, … by
their place among the query's aggregates) but `count(*)` and a `count(DISTINCT u)` the table
replaces by rows. The two are joined at the key. Conditions that read no FROM item, and the query's
own GROUP BY, HAVING, DISTINCT, ORDER BY, LIMIT and OFFSET, are taken over the join, each aggregate
replaced:

| as written, on side S | over the grouped sides |
|---|---|
| `count(*)` | `sum(rows_A × rows_B)` |
| `count(x)`, `sum(x)` | `sum(S's value × the other side's rows)` |
| `min(x)`, `max(x)` | `min` or `max` of S's value |
| `count(DISTINCT u)`, where `u`'s table reaches every FROM item of side A | `sum(rows_A)` |
| `count(DISTINCT u)`, otherwise | `sum` of side A's `count(DISTINCT u)` |

Each sum is taken as `numeric` and cast back to the aggregate's own type, so it is exact.

### The read it cancels

As written, the join makes a row for each pair of a side A row and a side B row with the same key,
and every pair is grouped. Respelled, each side is grouped first, so the join pairs one row for each
group of side A at a key with one for each group of side B at it: the pairs as written are never
made.

### In EXPLAIN

`CTE side_a` is side A's grouping, an Aggregate over side A's tables alone. Side B's grouping is an
Aggregate over side B's tables that reads none of side A's. The query's own grouping is taken over
the join of a `CTE Scan on side_a` and side B's grouping.

### When it applies

It applies where every condition below holds, each read from the parsed query and the catalog.

1. The query is a SELECT with GROUP BY and aggregates, without grouping sets, window functions,
   set-returning functions in its target list, set operations, row locks or a data-modifying WITH
   query, with no volatile function anywhere in it, and it reads no column or aggregate of a query
   around it.
2. Every join is an inner join, and the join conditions and WHERE together are the query's
   conditions. Its FROM items are read through:
   - a WITH query of its own that the statement reads once, a SELECT that is not recursive, not
     `MATERIALIZED`, holds no data-modifying WITH query and calls no volatile function, is read in
     its place;
   - a subquery in FROM that is not LATERAL, not a view, not behind a security barrier or row
     security, holds no LATERAL item and is a plain SELECT is read as the FROM items it joins.

   Any other WITH query or subquery stays one FROM item. After that there are at least two FROM
   items, each one the rules read, and neither the target list nor HAVING holds a subquery.
3. Every group key reads a column.
4. The aggregates are `count(*)`, `count(x)`, `sum(x)` of `int2`, `int4`, `int8` or `numeric`,
   `min(x)`, `max(x)` and `count(DISTINCT u)` with `u` a column, each from `pg_catalog`, with no
   FILTER, no ORDER BY inside it, not variadic and not an ordered-set aggregate, and each but
   `count(*)` reads a column. Any other aggregate, `avg` or a `sum` of `float8` among them, leaves
   the query as written.
5. The key is a set of columns the query's equalities make equal, each equality naming two columns
   of two FROM items. It holds columns of at least two FROM items, each column of a type with a
   default ordering.
6. The cut: the FROM items are joined into parts by every condition but the equalities of the key,
   and by what each group key and each aggregate reads, so that each reads one part. There are at
   least two parts. Side A is every part holding the column of a `count(DISTINCT u)`, or, where
   there is none, the part holding the first aggregate that reads a column; side B is every other
   part.
   - At least one aggregate reads a column of each side.
   - The key has a column on each side.
7. For each `count(DISTINCT u)`:
   - `u` is a column of a table, NOT NULL and alone a unique key of that table;
   - from `u`'s table the key is reached, one table of side A at a time. A table is reached where
     its columns that equalities hold equal to columns of tables already reached cover a unique key
     of it, NOT NULL in every column; once a table holding a column of the key is reached, each
     table's own columns of the key count among those. The key is reached when a table holding a
     column of it is.
8. Outside its aggregates, the target list and HAVING read a column only inside a group key: a
   column PostgreSQL admits because its table's primary key is a group key leaves the query as
   written.

Each set of equal columns gives one cut; of those that pass, one whose side A has the fewest FROM
items is taken. *A plain SELECT*, *a FROM item the rules read*, *an equality* and *a default
ordering* are set out in [The queries it reads](#the-queries-it-reads), and *a unique key* and
*NOT NULL* in [What it reads from the catalog](#what-it-reads-from-the-catalog).

## Rule 3: side B is grouped only at the keys side A holds

### What it changes

Side B's grouping gains the condition `k IN (SELECT key FROM side_a)`, `k` being side B's column of
the key: `side_a` is read a second time, and B is grouped only at the keys side A holds.

### The read it cancels

A row of side B at a key side A does not hold joins nothing. Without the condition, side B is
grouped over all its rows, and the join drops the groups at keys side A does not hold. With it,
those rows are dropped before B is grouped; where PostgreSQL reads B through an index on its key, at
`side_a`'s keys, they are never read.

### In EXPLAIN

Under side B's Aggregate, beside B's tables, a `CTE Scan on side_a` gives the keys B is kept to.
It is the second scan of `side_a`; the first is in the join of the two sides.

### When it applies

It applies wherever a grouping is taken on each side first, and checks nothing of its own. The two
sides are joined by an inner join at the key, so dropping a row of B whose key side A does not hold
changes no row of the answer.

## Using it

```toml
[dependencies]
pgrx = "=0.19.2"
warren-pg-speller = { version = "0.1", default-features = false }

[features]
pg18 = ["pgrx/pg18", "warren-pg-speller/pg18"]
```

```rust
#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    warren_pg_speller::init();
}
```

`init` puts the hook in place and keeps any planner hook already installed: the respelling runs
first and hands the statement, respelled or as written, to that hook, or to PostgreSQL's standard
planner where there is none. A statement is respelled only in a session that has loaded the
extension's library before the statement is planned: `shared_preload_libraries` makes sure of it for
every session, `session_preload_libraries` for the new sessions of the database or role it is set
on, and `LOAD` for one session.

It needs PostgreSQL 18 and pgrx 0.19.2. The `pg18` feature is the default; `pg19` builds the hook
for PostgreSQL 19's planner signature, as pgrx 0.19.2 binds its beta 2, and is not yet tested.

## The queries it reads

An INSERT, UPDATE, DELETE or MERGE is planned as written, every query inside it too. Every query of
a SELECT is read, each before the query around it: its subqueries in FROM, its WITH queries that are
SELECTs, and its subqueries in expressions. A recursive WITH query's own union is left as written;
the queries inside it are read.

The rules' conditions use these terms.

- **A FROM item the rules read** is a table, a view or subquery, or a WITH query other than a
  recursive WITH query's reference to itself, and is not LATERAL. A function, a VALUES list or any
  other FROM item leaves its query as written.
- **A plain SELECT** joins its FROM items by inner joins alone, and has no grouping or HAVING,
  aggregate, window function, DISTINCT, ORDER BY, LIMIT or OFFSET, set operation, row lock, WITH
  query of its own, set-returning function in its target list, or volatile function.
- **An equality** is an operator between two expressions that is the default equality of both sides'
  types, the one GROUP BY uses, under a deterministic collation or none. `point` has no default
  equality, so a join by `<@` between a `point` and a `box` is a condition other than an equality.
- **A default ordering** is the one ORDER BY uses for a type. `xid` has a default equality and no
  default ordering.

## What it reads from the catalog

While a statement is planned it reads the catalog: each type's default equality and ordering,
whether each collation it compares under is deterministic, each aggregate's name and schema, and,
for each `count(DISTINCT u)`, the unique keys and NOT NULL columns of the tables of side A, once for
each statement.

- **A unique key** is the key columns (INCLUDE columns apart) of a unique B-tree index that is
  valid, not deferrable and not partial, on columns rather than expressions, each under its type's
  default operator family and, where the column's collation is not deterministic, under that
  collation. The index of a PRIMARY KEY or UNIQUE constraint that is not DEFERRABLE is one. A table
  with inheritance children read without ONLY (a partitioned table apart), or read with TABLESAMPLE,
  has none here, and neither has a FROM item that is not a table.
- **NOT NULL** is a validated NOT NULL constraint, a primary key's among them; a
  `NOT NULL ... NOT VALID` constraint does not count.

Apart from unique keys, the indexes a table has play no part in any rule: the tests respell the
same question with primary keys alone, with B-trees, and with surveyors.

A plan made from a spelling rests on the keys it read, so PostgreSQL makes it again when one of
those tables changes in the catalog: a prepared statement whose table loses its primary key is
planned as written at its next run, and respelled once the key is back.

## When a spelling fails

Once a rule starts to build a spelling, the building and the planning of the respelled statement run
inside an internal subtransaction, one for the statement. Where either raises an ERROR other than a
cancel, the subtransaction is rolled back, the statement is planned as written, and the error is
logged at `DEBUG1` as `warren-pg-speller: planned as written, the respelling failed: <message>`. A
cancel, `statement_timeout` among them, rolls it back and cancels the statement. An ERROR raised
while the rules are still being checked, before the first spelling starts to be built, fails the
statement. A statement no rule starts to build takes no subtransaction.

Nothing is respelled inside a parallel operation: a statement planned during one is planned as
written.

## Where its tests run

pgrx runs a crate's `#[pg_test]`s only in the extension it builds, so the rules' tests run in
`warren_surveyor_pg`, in `warren-surveyor-pg/src/respelling/`: one module for each rule, one for a
question that reaches all three, one for what a respelled statement reads, as whom, what its plan
rests on, and how it is planned when its spelling fails, and one that makes the tables the tests
run on.

## License

Copyright (c) 2026 Kenneth Allen Flegal

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the
work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
