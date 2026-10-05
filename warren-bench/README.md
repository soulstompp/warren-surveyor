# warren-bench

Ask the same questions of a database under different sets of its indexes, and see what each answer
cost: the pages each plan read from outside shared buffers and the temp pages it wrote, then the
time. Every answer is checked against one fixed beforehand, so no figure stands for an answer that
was wrong.

It is the kit's runner. The walkthrough, from an empty server to the result table, is
[the kit](../kit/README.md). This page is the bench's reference, in the order a run uses each part:
installing it, what it touches, the questions, the targets, the index sets, the repetitions, the
files it writes and its exit status.

## Install

```sh
cargo install --locked warren-bench --root ~/.local
```

It needs Rust 1.96 or later, and puts `warren-bench` in `~/.local/bin`, which must be on your
`PATH`; without `--root ~/.local`, in `~/.cargo/bin`. The bench carries the kit inside it, the
questions and the surveyor script (*Questions*, below), and its own index sets, so every command
on this page runs from any directory. `warren-bench --version` prints `warren-bench 0.1.0`.
Installing touches no database. `cargo uninstall --root ~/.local warren-bench` takes it back out;
its cache directory, `$XDG_CACHE_HOME/warren-bench` (else `~/.cache/warren-bench`), you remove by
hand.

## What it touches

The bench reads, and writes nothing to a database. On each target, it:

- connects as the user the target's URL names, to the database it names, and to no other;
- has PostgreSQL parse each question before anything runs, and refuses, by its name, any that is
  not one statement returning rows, so no question can end a transaction or send a second
  statement;
- runs every repetition as
  `BEGIN; DROP INDEX …; SET LOCAL transaction_read_only = on; <the question>; ROLLBACK`, the
  `DROP INDEX` statements only under an index set, so a question that would write fails as an
  `ERROR`, and every index a set turns off is back when the repetition ends. The `ROLLBACK` is
  sent first on every way out; a session where it fails is closed, and PostgreSQL rolls back what
  a closed session left open. Nothing is ever dropped outside that transaction;
- changes no setting beyond its own sessions', and never starts, stops or reconfigures a server.

A `DROP INDEX` holds its table's lock until the repetition's `ROLLBACK`, and every statement of a
repetition waits at most 10 s for a lock. On this machine, the bench writes its cache directory
and the files you name. So it needs no server of its own: give it a database of its own, as the
kit does, and it leaves the server's other databases alone.

## In one minute

```sh
T="postgres://USER@HOST:5432/lego?options=-c%20search_path%3Dlego%2Cpublic"

# the expected answers, once: every question run twice, and required to agree
warren-bench truth --target lego lego.lego_purchases "$T" --out runs/expected.parquet

# every question under two index sets: every surveyor dropped, then every surveyor on
warren-bench run --index-sets dba,all --target lego lego.lego_purchases "$T" \
    --expected runs/expected.parquet --out runs/run-1
```

`runs/run-1/summary.txt` then gives, per question and index set, the pages read and written first
and the times after.

## Questions

The questions are a directory, `--questions`. Without it, the bench runs the kit's 25 questions,
which it carries inside it with their bound values. It writes them once into its cache directory,
`$XDG_CACHE_HOME/warren-bench` (else `~/.cache/warren-bench`), under `kit/<fingerprint>/`, where
the fingerprint is that of the files it carries, and leaves them read-only; every run reads them
there as it reads any directory. Expected answers recorded on those files hold wherever the same
files are read from.

`warren-bench kit write DIR` writes the same files into `DIR`, `questions/`, `binds.tsv` and
`surveyor.sql`, to read or edit, and writes nothing where any of them exists already;
`--questions DIR/questions --binds DIR/binds.tsv` then runs them. `warren-bench kit surveyor-sql`
prints `surveyor.sql`, the script that turns the surveyor on or off for a whole database, for
`psql -f -` ([the kit](../kit/README.md) runs it in its steps 2 and 5).

A questions directory holds plain `.sql` files or `.sqlc` templates, never both. Each file is one
question, named by its path without the extension (`single/01_purchases_per_day_december_2020`),
and the questions are taken in the order of their names.

- **Plain `.sql` files** are sent as written. A question with bound values marks them `$1`, `$2`,
  …, and the binds file gives them.
- **`.sqlc` templates** are composed once per target with sql-composer's `cargo sqlc compose
  --skip-prepare`, then checked with `--verify`. A template reads the target's table through
  `:compose(target/table.sqlc)`, which the bench writes for each target as `SELECT * FROM <the
  target's table>`; every target's composed questions must be the same text but for the table's
  name. A template with an open slot (`:compose(@slot)`) is a shape and no question.

Each target's questions, composed or plain, are written under `--work`, by default `compose` in the
cache directory.

The binds file, `--binds` (by default the kit's), has a header line, then one line per value:
`question`, `name`, `value`, separated by tabs. A question takes its values in the order of their
names, so `$1` is the first name. A plain question's placeholders must be exactly `$1` to `$N` for
its N values. A bound question is prepared (untimed), executed with its values written as literals,
and deallocated.

The question set has a fingerprint over every file of the directory and the binds file. `run` and
`check` refuse expected answers made for another set, so a changed question needs new expected
answers.

## Targets

```sh
--target NAME TABLE postgres://USER@HOST:PORT/DATABASE
```

A target is a name, a table and a URL (`postgres://` or `postgresql://`), and `--target` may be
repeated, no two with one name. Targets come from the command line only. The URL must name its
host, port and database, so that none of them is filled in from the environment, and a password is
shown as `***` in every log and file. The server must be PostgreSQL 13 or later.

The table is what the target is recorded by: its sizes, its partitions and its statistics, in
`targets.parquet`. Its partitions, or the table itself where it has none, are the target's
**leaves**: the plans' scans are counted on them, and the rows witness reads them (below).

A question names its tables without a schema, so the target's `search_path` says which tables it
reads. Set it in the URL's `options`: `…/lego?options=-c%20search_path%3Dlego%2Cpublic` reads the
`lego` tables, and `…search_path%3Dlego_oo%2Clego%2Cpublic` reads the `lego_oo` classes first. Keep
`public` in the path where the extensions (`cube`, `earthdistance`, `pg_trgm`) live.

A database that holds a surveyor must load `warren_surveyor_pg` as every session starts
(`session_preload_libraries` or `shared_preload_libraries`), so that every session plans alike; a
target where it does not is refused, with the setting that clears it.

## Index sets

`--index-sets a,b,…` runs every target under each set: the target `lego` becomes `lego@a`,
`lego@b`, each a target of its own. Each repetition under a set drops the indexes the set turns off
inside the transaction it rolls back (*What it touches*), so they are off for that repetition
alone. Where its `ROLLBACK` fails and its session is closed, its target's other repetitions of the
question are recorded as not run.

The sets are declared in `--index-set-file`, by default the bench's own, which it carries:

| set | turns off |
|---|---|
| `all` | nothing: the DBA's indexes with each table's surveyor |
| `dba` | every surveyor: the DBA's indexes, with the library still loaded |
| `keys` | every index that backs no constraint, the surveyors among them |
| `without:<key>` | one key's indexes, and nothing else |

A set names the indexes it turns off by selectors, each of any of `method` (the access method),
`constraint` (true for an index behind a primary key, unique or exclusion constraint), `expression`
(true for an index with an expression among its keys), `schema`, `table` (the index's table, or
the table at the top of its inheritance) and `key`; an index matches a selector when it matches
every field given. A family, such as `without`, is a set per key: `<family>:<key>` turns off what
its base set turns off, and the key's indexes its selectors match. Set and family names are
lower-case letters, digits, `-` and `_`, and a field the file does not know is refused.

A **key** is one index definition, its text after `USING <method>`, on every table of one name and
every table under it by inheritance or partitioning, in every schema. It is named by that name and
its index's name without the table's name and `_idx`: `lego_sets_theme_id_set_num` holds
`lego_sets_theme_id_set_num_idx` on `lego.lego_sets` and `lego_sets_town_theme_id_set_num_idx` on
the class `lego_oo.lego_sets_town`. An index behind a constraint, and a surveyor, belong to no key.

A set that would drop a constraint's index is refused, and so is a set that turns nothing off where
it declares something to turn off (`dba` on a database with no surveyor). A partition's index that
belongs to its partitioned table's is turned off with it, by dropping the partitioned table's; a set
that would turn one off alone is refused, since PostgreSQL drops them only together.

Before anything runs, each `<target>@<set>` is certified: the set's transaction is opened, the
indexes are read inside it, and they must be the committed ones less those the set turns off. If
the committed indexes change during a run, the run stops. Every scan of every plan must use an
index its set held; one that did not is written to `unlawful.txt` and sets the exit status to 5.

## Repetitions

| subcommand | does |
|---|---|
| `truth` | runs every question twice on one reference target, under at most one index set, each time in a new session; requires the same answer both times, and writes the expected answers to `--out`, which it refuses to overwrite without `--force` |
| `run` | runs every question on every target, `--cold` repetitions (3) each in new sessions and `--warm` repetitions (5) in the last one's, checks every answer against `--expected`, and writes into the directory `--out` |
| `check` | runs each question once on every target, in a new session, and checks the answer against `--expected`, with no time and no plan; `--only PREFIX` keeps the questions whose names start with it, and `--out` names the file it writes, which `--force` replaces |
| `compose` | prepares the questions for every target and certifies any index sets, prints each target's directory of composed questions, then stops |
| `sample NAME` | a few questions on a few index sets, with expected answers the bench computes once; a quick check, never a measurement (below) |

`run` takes the questions in order. For each it takes the targets in an order rotated by one per
question, so no target always goes first. A repetition is the statement, timed at the client from
sending it to its last row, then, in a second session, its
`EXPLAIN (ANALYZE, BUFFERS, SUMMARY, MEMORY, VERBOSE, FORMAT JSON)` (`MEMORY` from PostgreSQL 17
on). "Cold" is the question's first run in a new session, after the session's settings and the
statements that open the repetition; the buffer cache is not cleared, and the pages found and read
say where the data came from. After a repetition that does not finish, its target's other
repetitions of the question are recorded as not run.

`--plan-cache-mode` sets `plan_cache_mode` for every session: `auto`, `force_custom_plan` or
`force_generic_plan`. A bound question is prepared afresh in each repetition, so under the
server's default it always gets a custom plan; `force_generic_plan` measures the plan a long-lived
statement would reuse.

`--derived DIR` (on `run` and `check`, and repeatable) adds statements another program wrote for
each target, each answering as one of the questions. `DIR` holds one directory per target, named as
`--target` names it, each with the same files `DIR/<target>/<question>.sql`. Each statement is
recorded as `<DIR's name>/` followed by its question's name less the first directory, and is
checked against that question's expected answer, with its bound values.

`sample NAME --target … --samples DIR --out DIR --lock FILE` runs the sample declared in
`<samples>/NAME/sample.json`: `questions` and `binds` (paths from the sample's directory), `only`
(name prefixes; every question when empty), `index_sets` (at least one), `cold` and `warm` (which
`--cold` and `--warm` override) and `statement_timeout`. Its targets read one database. Its
expected answers are computed once, on the first target under the first set, into
`<out>/NAME/expected.parquet`, and are refused on another database; each sample writes into
`<out>/NAME/<UTC time>/`. It refuses to start while the `--lock` file exists, never takes it, and
stops before its next question if it appears. It takes `--index-set-file`, `--work` and the memory
flags below; everything else comes from the sample file.

### Safety

- One statement at a time, every session named `application_name = 'warren-bench'`, every
  statement the questions send under `--statement-timeout` (`120s`: digits and one of `ms`, `s`,
  `min`, `h`, `d`), which also ends a session left idle inside a transaction.
- While a statement runs, a control session reads the backend's resident memory (with its parallel
  workers') every `--watch-interval-ms` (100), cancels the statement above `--rss-cap-mb` (2048),
  and terminates the backend if it is still running `--terminate-after-s` (30) later. Ctrl-C
  cancels it the same way. The memory is read from `/proc`, so the bench refuses a server whose
  backends it cannot see there unless given `--no-memory-watchdog`.
- The bench does not start, and a run stops, while the generator is loading: a session on the
  server whose `application_name` starts with `sqlc-brickgen`.

## What the bench writes

Every file meant for a program is Parquet, and every column has a declared type. A value that does
not fit its type stops the program, a missing value is null, and a reader refuses a file whose
columns are not exactly the declared ones. The logs go to standard error, as text or, with
`--log-format json`, as JSON.

`truth` writes one line per question: its rows and digest, the question set's fingerprint, and the
target it was made on. `check` writes one line per (target, question): a fingerprint of the
statement sent, its status and verdict, its rows and digest beside the expected ones, and any error.

`run` writes into `--out`, in this order. Give every run a directory of its own: a run refuses one
that already holds `results/` or `leaves/`, before it writes anything into it.

- `targets.parquet`: one line per (target, key): the URL, the server's version and address, the
  user, the server's settings (`setting.*`), the database's locale (`database.*`), the
  extensions and their versions, the sha256 of each library its sessions preload (read where the
  server is on this machine), the `EXPLAIN` it is sent, the table's partitioning, sizes and
  statistics dates, a fingerprint of every index of the database (`roster`), and under a set the
  set and the indexes it turns off (`index_set`, `indexes_off`). A fingerprint of all of it but the
  bench's own session settings, `config_id`, is on every results line.
- `indexes.parquet`: every index of the database's own schemas, for every target under each index
  set it ran (`index_set`, empty without one): its table, method, key parts, `INCLUDE` columns,
  predicate, constraint, whether it has an expression, whether it is valid, its size, and whether
  the set held it present.
- `results/`: one file per question (`part-0001.parquet`, …), one line per (target, question, cold
  or warm, repetition):
  - the repetition: `target`, `config_id`, `question` (its first directory as `spelling`, the rest
    as `selection`), `phase`, `rep`, `session`, and `position`, the target's place in the
    question's order;
  - the answer: `status` (`OK`, `TIMEOUT`, `CANCELLED`, `ERROR` or `NOT_RUN`), `verdict` (`RIGHT`,
    `WRONG`, or `-` where the statement did not finish), the client's `wall_ms`, and `rows` and
    `digest` beside `expected_rows` and `expected_digest`;
  - memory: the backend's peak with its workers' (`peak_rss_kb`, `peak_anon_kb`), and the
    EXPLAIN's (`explain_peak_rss_kb`);
  - the EXPLAIN: `explain_status`, `planning_ms`, `execution_ms`, the rows it returned
    (`explain_rows`, `explain_rows_ok`) and the planner's estimate of them (`plan_rows`), the pages
    found in shared buffers (`shared_hit`), read from outside them (`shared_read`), dirtied and
    written, the temp pages read and written, planning's pages (`planning_shared_hit`,
    `planning_shared_read`), planner memory used and allocated, `jit_ms`, `workers_launched` and
    `subplans_removed`;
  - the leaves: how many the plan held and ran (`leaves_planned`, `leaves_executed`), its scans of
    them and of other relations (`leaf_scans`, `other_scans`), a fingerprint of which it held
    (`leaf_set`), and the rows its EXPLAIN reports reading on them and elsewhere
    (`explain_leaf_rows_read`, `explain_other_rows_read`);
  - the rows witness (below): `witness_status`, the leaves, rows and pages the counters saw
    (`witness_leaves`, `witness_leaf_rows_read`, `witness_leaf_blocks`, `witness_other_rows_read`,
    `witness_other_blocks`), the rows planning read (`plan_leaf_rows_read`,
    `plan_other_rows_read`), whether execution and its EXPLAIN agree (`rows_read_agree`,
    `leaves_agree`, `leaves_disagreement`), and the relations each read beyond the leaves
    (`witness_other_relations`, `plan_other_relations`, `explain_other_relations`);
  - `error`.
- `leaves/`: one file per question, one line per scan node of each target's first EXPLAIN of the
  question: its place in the plan (`node`), its type, schema, relation and index (a bitmap heap
  scan's indexes joined by `+`), whether the relation is a leaf, its loops, its rows over every
  loop, the planner's estimate for one loop (`plan_rows`), its index searches, its index condition
  and its heap fetches.
- `summary.parquet`: per (question, target, cold or warm), the verdict and the count of each
  outcome, then, over the repetitions that answered RIGHT, the answer's rows, the client's time
  (median, minimum, maximum and median absolute deviation), planning and execution time (median,
  minimum and maximum), the medians of planner memory, peak memory and the pages found and read,
  the leaves planned and executed and the scans of them, and the errors.
- `summary.txt`: the summary as grids, questions down and targets across. After every answer,
  **the pages come first**: those read from outside shared buffers and found in them, and the temp
  pages written; then the leaves, and only then the times (the client's, warm and cold, planning
  and execution), planner memory and the rows returned. It ends with each target's sum of warm
  medians. A WRONG answer, a timeout, a cancel or an error is counted and shown in place of a
  figure (`WRONG 1/5`), never averaged in.
- `rows.txt`: per (question, target), over the repetitions that answered RIGHT, the answer's rows
  beside the planner's estimate, the rows the statement read on the leaves, the rows planning read,
  the rows its EXPLAIN reports reading, the rows read per row of the answer, the leaves touched
  against those planned, and the median warm and cold times.
- `scans.txt`: from `leaves/`, per question and relation, every target's scans of it side by side:
  each scan's type and index, the planner's estimate for one loop against the actual rows of one
  loop, its loops and its heap fetches.
- `unlawful.txt`: where a scan used an index its set turned off, one line for each.

`summarize --results` prints the summary of a results file or of a run's `results/`. `export
--from … --to …` writes any file the bench wrote, or a directory of them, as tab-separated text: a
header, then the rows sorted by the file's key and then by every other column, nulls as empty
fields, and `\\`, `\t`, `\n`, `\r` escaped; it refuses an empty text, which it could not tell from
null. `import --from … --to …` reads that text back into Parquet, and exporting the result gives
the same text again.

**The digest.** An answer is certified by its row count and a digest of its rows. A boolean, an
integer, a `real`, a `double precision`, a `numeric`, a `time`, a `timestamp` and a `timestamptz`
are each read in one canonical text, however the session prints them; every other value, a date and
an `interval` among them, is read as the server sends it, so the expected answers and the run need
the same `DateStyle` and `IntervalStyle`. The digest does not depend on
row order, and it changes when a row is added, removed, repeated or altered. A RIGHT answer also
needs its EXPLAIN to return the same number of rows. A timing counts only with a RIGHT answer.

**The rows witness.** The EXPLAIN runs in another session, so the bench also reads the server's
per-table counters around the timed statement itself, and around a plan-only EXPLAIN to take off
what planning read. The rows the execution read must equal those its EXPLAIN reports, within the
rounding of EXPLAIN's per-loop figures, and the leaves each scanned must agree; a repetition that
breaks this sets the exit status to 4. The witness needs PostgreSQL 15 or later with `track_counts`
on, and is asked only of readings that settled with no other session connected to the database;
the bench logs how many repetitions it was not asked of, and why.

## Exit status

`0` every answer RIGHT; `1` some answer WRONG; `2` refused, or stopped by an error; `3` no WRONG
answer, but some repetition timed out, was cancelled, failed, did not run or had its EXPLAIN fail,
or the run stopped early, for the generator, a change in the committed indexes or a sample's lock;
`4` every answer RIGHT, but an execution and its EXPLAIN disagree on the rows read or the leaves
scanned; `5` no WRONG answer, but a scan used an index its set turned off; `130` interrupted.
`check` exits with `0`, `1`, `2`, `3` or `130`; `summarize` with `0` to `4`, for the results it
reads; `truth`, `compose`, `export`, `import` and `kit` with `0` or `2`.

## Tests

```sh
cargo test -p warren-bench
```

The tests against a server run only when asked, and only with this set:

```sh
BLOCKER_DB_URL=postgres://USER@localhost:5432/postgres \
    cargo test -p warren-bench -- --ignored --test-threads=1
```

They need a superuser's URL, the server on this machine (they read its backends' memory), and
`warren_surveyor_pg` installed on it. Each makes a small database of its own and drops it when it
ends: `warren_bench_sets_check`, with small `lego` tables, a class hierarchy of their sets and a
surveyor on each, or `warren_bench_check`, with one table of the types an answer's digest reads.

## Licence

`MIT OR Apache-2.0`, at your option: [LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE).
Copyright Kenneth Allen Flegal.
