# warren-surveyor

The surveyor measures without digging.

It is an index for PostgreSQL 18 that is never scanned and holds nothing. While a statement is
planned, it reads the indexes you already have and hands the planner what they measure, where the
planner would otherwise estimate and scan on the guess. You see it in `EXPLAIN (ANALYZE, BUFFERS)`,
each scan's estimate beside its actual rows; its own reads draw on one budget, the pages of the
tables the statement reads, and where they stop, PostgreSQL's own estimate stands.

| crate | for |
|---|---|
| `warren-surveyor-pg` | PostgreSQL 18: the extension `warren_surveyor_pg` and its index access method, `surveyor` |
