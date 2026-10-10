# Changelog

## 0.1.1

- PostgreSQL 14, 15, 16 and 17 as well as 18, each a Cargo feature, `pg14` to `pg18`; `pg18` is the
  default. The server must be 14.19, 15.14, 16.10, 17.6 or later.
- On 14 and 15, building a surveyor warns that an UPDATE changing one of its columns is no longer
  HOT. From 16 on, a surveyor never keeps an update from being HOT, as before.
- Before 17, `warren_key` is collated by `"C"`, which orders UTF-8 text by code point as
  `pg_c_utf8` does from 17 on.
- Before 18, the B-trees are priced as that version prices them: with no skip scan, a path with no
  condition on its leading column walks the whole index.
- `ALTER EXTENSION warren_surveyor_pg UPDATE` takes a 0.1.0 install to 0.1.1.
- The respelling of `warren-pg-speller` 0.1.2, installed with the library.

## 0.1.0

The first release: the surveyor for PostgreSQL 18, an index that is never scanned and holds
nothing. You put one on a table, on the table's own unique key; while a statement is planned, it
reads your indexes and hands the planner what they measure.

- The rows a table's constant conditions select, measured on its B-trees.
- The rows of a text search or a trigram pattern, measured on its GIN indexes.
- The rows in a box or within a distance, measured on its GiST indexes of cubes and earths.
- The rows in all of a table's conditions at once, where two or more of its B-trees and GIN indexes
  hold them.
- The column statistics joins and groupings are sized by, measured on the B-trees a column leads.
- The rows a `WITH RECURSIVE` query returns, read by running it while the statement is planned,
  with the values of an integer or date column it is joined on.
- The price of your B-trees, from the pages of them in shared buffers and the order of their
  leaves on the disk.
- The price of every page a statement would read from the disk, weighed by how busy its drives are.
- A grouping within one value's reach made by hashing, never by sorting.
- Every read for a statement held to one budget, the pages of the tables it reads, which
  `warren_surveyor_pg.planning_read_limit` lowers.
- `warren_surveyor_pg.walked_leaf_page_cost`, the price of a leaf a scan walks in key order, as
  measured on your drive.
- What each read measured, and where the budget stopped one, noted at `DEBUG1`.
- A surveyor holds no page, writes nothing to the WAL for a new row, and never keeps an update
  from being HOT.
- Operator classes for 17 key types, and the domain `warren_key`, text ordered by code point.
- The respelling of `warren-pg-speller` 0.1.1, installed with the library.
