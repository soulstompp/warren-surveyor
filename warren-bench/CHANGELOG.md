# Changelog

## 0.1.0

The first release: the kit's runner. It asks the same questions of a PostgreSQL database under
different sets of its indexes and records what each answer cost, every answer checked against one
fixed beforehand.

- Installs with `cargo install --locked warren-bench`, and carries the kit inside it: its 25
  questions on the LEGO catalogue, their bound values, and `surveyor.sql`.
- Runs the kit's questions where none are given, from its cache directory
  (`$XDG_CACHE_HOME/warren-bench`, else `~/.cache/warren-bench`); `--questions` and `--binds` give
  others, plain `.sql` files or `.sqlc` templates composed for each target.
- `kit surveyor-sql` prints the script that turns the surveyor on or off for a whole database, for
  `psql -f -`; `kit write DIR` writes the kit's files out to read or edit.
- `truth` records each question's expected answer, run twice and required to agree; `run` runs
  every question cold and warm on every target and checks every answer; `check` checks answers
  without timing them; `sample` is a quick check, never a measurement.
- Each question is parsed by PostgreSQL before anything runs, and refused unless it is one
  statement returning rows; each repetition runs inside a transaction the bench rolls back,
  read-only from the question on, so no question writes a row.
- Index sets turn indexes off for one repetition, inside a transaction that rolls back: `all`,
  `dba`, `keys` and `without:<key>`, or the sets of `--index-set-file`.
- Each repetition's pages read from outside shared buffers and found in them, its temp pages, its
  planning, its scans and its time, from its `EXPLAIN (ANALYZE, BUFFERS)` in a second session.
- An answer is certified by its row count and a digest of its rows, whatever their order.
- Every file it writes is typed Parquet; `export` and `import` turn any of them into text and back.
- A statement timeout, a watchdog on the backend's memory, and a stop while the generator loads.
