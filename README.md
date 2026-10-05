# warren-surveyor

The surveyor measures without digging.

It is an index for PostgreSQL 18 that is never scanned and holds nothing. While a statement is
planned, it reads the indexes you already have and hands the planner what they measure, where the
planner would otherwise estimate and scan on the guess. You see it in `EXPLAIN (ANALYZE, BUFFERS)`,
each scan's estimate beside its actual rows; its own reads draw on one budget, the pages of the
tables the statement reads, and where they stop, PostgreSQL's own estimate stands.

Install the bench from crates.io, and the extension from a clone of this repository against your
server's `pg_config`:

```sh
cargo install --locked warren-bench --root ~/.local
cd warren-surveyor-pg && cargo pgrx install --release --pg-config "$PG_CONFIG"
```

Neither changes a database or a setting. *Install* in [the kit](kit/README.md#install) gives what
each needs first, where it lands, how to see it worked and how to take it back out.

[Its README](warren-surveyor-pg/README.md) gives each read, the scan it cancels, how it shows in
`EXPLAIN` and where it stops, then every setting. [The kit](kit/README.md) reruns its benchmark on
your own server with one command, `kit/run.sh`: it loads a LEGO database built to trip indexes,
runs 25 questions on PostgreSQL alone, on the respelling alone and with the surveyor, the DBA's
indexes the same on each, and compares the pages each reads from outside shared buffers, then the
time, every answer checked by its row count and a digest of its rows.

| crate | for |
|---|---|
| `warren-surveyor-pg` | PostgreSQL 18: the extension `warren_surveyor_pg` and its index access method, `surveyor` |
| `warren-pg-speller` | PostgreSQL 18: a planner hook that plans a `SELECT` through another spelling of it that returns the same rows; `warren_surveyor_pg` installs it |
| `warren-bench` | runs a set of questions on a database under sets of its indexes, and records each one's pages, time and plan, and whether its answer is right; the kit's runner, with the kit inside it: `cargo install --locked warren-bench` |

`warren-surveyor-pg` is licensed under the GNU General Public License, version 3 or later
([LICENSE](LICENSE)). The speller, the bench and the kit are licensed under either of the Apache
License, Version 2.0 or the MIT license, at your option; each holds both texts.
