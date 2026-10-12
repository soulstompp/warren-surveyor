# Changelog

## 0.1.1

- `--with` and `--respell` compose a directory of `.sqlc` templates with those flags of
  `cargo sqlc compose`, and the question set's fingerprint follows them. Plain questions refuse
  both.
- `--search-path DIR`, given once for each directory, composes the templates with relations from
  `DIR` too, which are not questions themselves. The binds are checked through them, and the
  fingerprint follows their files. Plain questions take none.
- The `manifest.json` that `cargo sqlc compose` writes beside the composed files is no longer
  counted as a file the templates did not compose.
- The kit's questions that put others together are in `combined/`, so their names begin
  `combined/`; they were in `composed/`.
- Question 25, which puts most of the others in one, is in `in_one/`, so its name begins
  `in_one/`; it was in `proof/`.
- `suite/` holds the kit's 25 questions as `.sqlc` templates, each composing to the kit's own
  question byte for byte, and four more that join a question's rows to the bricks they brought,
  with the relations they compose in `suite/relations`, read with `--search-path`.
- The kit's `surveyor.sql` says that a role granted SET on `session_preload_libraries` may set it
  from PostgreSQL 15.
- The timed session is sent the plan the server ran its question by, through `auto_explain` with
  nothing counted per node, and the rows witness is asked only where the EXPLAIN planned the same
  tree. Where the EXPLAIN planned another, the results say the plan moved (`plan_moved`, both plans
  in `timed_plan` and `explain_plan`), the run lists each such repetition, and none sets exit
  status 4. `targets.parquet` says whether the plan is sent (`timed_plan`), and results written
  before read as they were recorded.

## 0.1.0

The first release: the kit's runner. It asks the same questions of a PostgreSQL database under
different sets of its indexes and records what each answer cost, every answer checked against one
fixed beforehand.

- Installs with `cargo install --locked warren-bench`, and carries the kit inside it: its 25
  questions on the catalogue of bricks and sets, their bound values, and `surveyor.sql`.
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
