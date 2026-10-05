# Changelog

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
