# Changelog

## 0.1.1

- Kept in the warren-surveyor repository, beside the surveyor that installs it.
- A unique key counts only where each of its columns is NOT NULL by a validated constraint and,
  where a column's collation is nondeterministic, its index is under that collation. A
  `count(DISTINCT u)` whose `u` is NOT NULL only by a `NOT NULL ... NOT VALID` constraint, or
  unique only by an index under another collation than its own nondeterministic one, is left as
  written instead of returning a wrong count.
- A join by an operator of a type with no default equality (a point within a box) is read as a
  condition other than an equality, and a column of a type with no default ordering (`xid`) is not
  taken as a key, instead of failing the statement with "could not identify an equality operator"
  or "could not identify an ordering operator".
- A respelled statement over a table whose row security policy holds a subquery returns the rows
  the policy admits, instead of failing with "cannot handle unplanned sub-select".
- An ERROR raised by a planner hook installed before this one unwinds the respelling's frames.
- A spelling is built and planned inside an internal subtransaction, begun only once a spelling
  starts to be built; where either raises an ERROR other than a cancel, the statement is planned as
  written and the error is logged at DEBUG1. Nothing is respelled inside a parallel operation.
- The WITH query and the subquery a respelled grouping adds are renamed `side_a` and `side_b`, and
  their aggregates' columns `side1`, `side2`, ….
- The README shows the dependency from crates.io.

## 0.1.0

The first release: a PostgreSQL planner hook that respells a SELECT through three rules before it
is planned, wherever the catalog proves them, whatever indexes the tables have.

- A join a UNION ALL's arms share is taken out of the union and made once.
- A grouping over a join is taken on each side first, and the sides are joined at the join key.
- The side the grouping does not count is grouped only over the keys the counted side holds.
