# The kit: rerun the surveyor's benchmark yourself

The surveyor is an index that is never scanned and holds nothing. You put one on a table. While a
statement is planned, it reads the indexes you already have (your B-trees, and the GIN and GiST
indexes that search words, trigrams, boxes and distances) and tells PostgreSQL's planner what they
measure. Its library also respells a `SELECT`: it plans it through another spelling that returns
the same rows and is written to read less. The point is to avoid the disk: fewer pages read from
outside shared buffers.

This kit lets you check that on your own server. One command, `kit/run.sh`, installs what it
needs and runs seven steps:

1. load a real LEGO catalogue, and a large made-up one built from it to trip indexes (skewed
   purchases, themes under many roots, names typed each builder's own way, clocks that change),
   with the indexes a DBA would give it;
2. turn the surveyor off;
3. record every answer with PostgreSQL alone;
4. time every question with PostgreSQL alone;
5. turn the surveyor on;
6. run every question again, with the surveyor and without it, each answer checked against the
   recorded one by its row count and a digest of its rows;
7. read the difference: pages read from outside shared buffers first, then time.

We would like to hear what you find, the losses most of all.

## Run it all

On the machine your PostgreSQL 18 server runs on:

```sh
git clone https://github.com/soulstompp/warren-surveyor
cd warren-surveyor
kit/run.sh --db postgres://USER@HOST:5432/lego --pg-config /path/to/pg_config
```

`--db` names the database the kit makes for itself on your server, as a superuser; it refuses a
database that exists. `--pg-config` is that server's `pg_config`. Before it changes anything, the
script checks every need in *What you need* below and cargo-pgrx 0.19.2, and names whatever is
missing with the command that installs it.

Then it runs the steps below in order. It builds the bench inside the clone, installs the generator
there too unless `sqlc-brickgen` is already on your `PATH`, and installs the extension into the
server's own directories (give `--sudo` on a server installed by a package manager): the one thing
it writes outside the clone, but the database. It makes and loads the database (step 1), turns the
surveyor off and records every answer with PostgreSQL alone (steps 2 and 3), times every question
with PostgreSQL alone (step 4), turns the surveyor on and times every question with it and without
it (steps 5 and 6), and turns it off again.

It ends by printing each run's summary, and leaves every file under `runs/<size>-<time>/` in the
clone: `run/summary.txt` and `alone/summary.txt`, which step 7 reads, and `facts.txt`, what ran on
what and how long each step took. Exit status 0 means every answer was RIGHT; the bench's README
lists the others. Where it stops, it names the step, and where the surveyor was on, the one command
that turns it off.

`--size medium` or `--size huge` loads more (huge took us 36 minutes and 197 GB); `--surveyor-only`
times only the surveyor's side; `--no-memory-watchdog` is for a server whose backends this machine
cannot see. The database stays when it ends: `DROP DATABASE lego` takes it away. Everything else
but the extension's files is inside the clone; the extension comes out as *Uninstalling* in
[the surveyor's README](../warren-surveyor-pg/README.md#uninstalling) says.

## Install

To run the steps one by one instead, install the bench, the generator and the surveyor's extension
once. They need Rust 1.96 or later; the extension also needs cargo-pgrx 0.19.2
(`cargo install cargo-pgrx --version 0.19.2 --locked`), your PostgreSQL 18's `pg_config`, and, on
a server installed by a package manager, its development package (the server's headers) and
libclang, which pgrx builds against.

```sh
cargo install --locked warren-bench --root ~/.local
cargo install --locked sqlc-brickgen --root ~/.local
```

The first installs the bench, `warren-bench`, which steps 2 to 6 run. It carries the kit inside it:
the questions and the script that turns the surveyor on and off. The second installs the
generator, `sqlc-brickgen`, which step 1 runs. Both land in `~/.local/bin`, which must be on your
`PATH`; without `--root ~/.local`, they land in `~/.cargo/bin`. `warren-bench --version` prints
`warren-bench 0.1.0`, and `sqlc-brickgen --help` its flags.

The extension builds from a clone of this repository, against your server:

```sh
git clone https://github.com/soulstompp/warren-surveyor
cd warren-surveyor/warren-surveyor-pg
PG_CONFIG=/path/to/pg_config
mkdir -p ~/.pgrx && printf '[configs]\npg18 = "%s"\n' "$PG_CONFIG" > ~/.pgrx/config.toml
cargo pgrx install --release --pg-config "$PG_CONFIG"
```

pgrx reads where your PostgreSQL is from `~/.pgrx/config.toml` (or `$PGRX_HOME/config.toml`): the
`printf` writes that file, and where it exists already, add the `pg18` line under its `[configs]`
instead. `cargo pgrx install` builds the extension `warren_surveyor_pg` and copies its library,
`warren_surveyor_pg.so`, into the directory `"$PG_CONFIG" --pkglibdir` prints, and its control
file and SQL into the server's extension directory: step 5 creates the extension from them. It is
the one install that writes into the server's own directories: on a server installed by a package
manager, add `--sudo`. To see that the server has it, ask:

```sql
SELECT name, default_version FROM pg_available_extensions WHERE name = 'warren_surveyor_pg';
```

It gives `0.1.0`. Installing changes no database and no setting: the library acts only in a
session that loads it, and step 5 is the one that has the kit's database load it. To take it all
back out: `cargo uninstall --root ~/.local warren-bench sqlc-brickgen`; the bench's cache directory,
`~/.cache/warren-bench` (or under `$XDG_CACHE_HOME`); and the extension, as *Uninstalling* in
[the surveyor's README](../warren-surveyor-pg/README.md#uninstalling) says.

## What it touches

The kit works in one database of its own, `lego`, which step 1 makes, and reads and writes no
other. It needs no server of its own: a server that holds your own databases will do.

- **The generator** drops and makes its own schemas, `lego`, `lego_oo`, `lego_inheritance`,
  `lego_range` and `lego_hash`, in the database it is given: give it the kit's database, never one
  of yours. It reads the real catalogue in `public` and never writes it, and creates there only the
  extensions its search indexes need, unless the database has them already.
- **The bench** runs the questions it is given, as the user its URL names, and writes no row. Before
  anything runs, PostgreSQL parses each question, and the bench refuses any that is not one
  statement returning rows. Every repetition runs inside a transaction the bench always rolls back,
  read-only from the question on, so a question that would write fails as an error. Under an index
  set, the `DROP INDEX` statements run inside that transaction, so no index is ever dropped. The
  bench changes no setting beyond its own sessions, and never starts, stops or reconfigures a
  server.
- **The surveyor script** changes only the database it is given, and only with `-v apply=1`.
  Turned on, it gives that database's tables a surveyor each and changes how its statements are
  planned, until turning it off drops the surveyors and the library's preload again; the extension
  stays.

An index set's `DROP INDEX` holds its table's lock until the repetition's `ROLLBACK`, and the
surveyor script the locks of the tables it changes until it commits; the first waits at most 10 s
for a lock, the second 30 s, and on the kit's database both lock only the kit's tables. On a busy
server, the times wobble and the pages each plan touches do not, which is why step 7 reads the
pages first.

## What you need

- **PostgreSQL 18**, a server you can create a database on, as a superuser: the load creates
  `earthdistance`, the surveyor's extension is one only a superuser may create, and turning it on
  sets the database's `session_preload_libraries`. Its `psql`, and the contrib extensions `cube`,
  `earthdistance` and `pg_trgm`.
- `curl`, and about 4 GB of disk for the small load, which takes under a minute.
- The bench on the server's own machine, on Linux: it reads each backend's memory from `/proc` to
  stop a statement before it takes the server's memory. For a server elsewhere, give `truth` and
  `run` `--no-memory-watchdog`. It connects without TLS.

Every command below names the database by URL. Write yours in once:

```sh
DB=postgres://USER@HOST:5432/lego
```

## Where the data comes from, and its terms

The kit ships no data: step 1 downloads the real catalogue to your machine, and `sqlc-brickgen`
builds the rest from it and from the cities it ships, Natural Earth's populated places (public
domain). The catalogue is Neon's sample dump, `lego.sql` in
[neondatabase/postgres-sample-dbs](https://github.com/neondatabase/postgres-sample-dbs), a
repository under the MIT licence, whose README gives the data's source as Kaggle's
[LEGO Database](https://www.kaggle.com/datasets/rtatman/lego-database) (rtatman), which Kaggle lists
under CC0. The data comes originally from [Rebrickable](https://rebrickable.com), and the kit
follows Rebrickable's terms: they allow it to be used for any purpose, commercial use included, ask
that it be credited as sourced from Rebrickable (credit Rebrickable wherever you use it), and forbid
using any Rebrickable content to train AI models.

LEGO® is a trademark of the LEGO Group, which does not sponsor, authorise or endorse this project.

## 1. Load the catalogue and the trapped database

```sh
psql -X -d postgres://USER@HOST:5432/postgres -c 'CREATE DATABASE lego'
curl -sSfL https://raw.githubusercontent.com/neondatabase/postgres-sample-dbs/main/lego.sql \
    | psql -X -q -v ON_ERROR_STOP=1 -d $DB
sqlc-brickgen --database-url $DB --size small \
    --oo-schema lego_oo --partitioning inheritance,range,hash
```

The first two make the kit's database and load the real catalogue into its `public` schema: eight
`lego_*` tables of colours, themes, part categories, parts, sets, inventories and their lines. The
generator reads them, writes them through into the schema `lego`, and adds the made-up catalogue:
20 thousand more sets at `small`, 2 million at `huge`, with their builders, the builders'
collections and their purchases, and every trap it knows. It also builds the DBA's indexes, the
ones every later step measures against: the primary and unique keys, composite indexes for the
joins holding in `INCLUDE` the columns the questions read, and an index for each search those do
not serve, a postcode by its prefix (a B-tree), a home by its distance (GiST), a part's name by its
words and a name by its trigrams (GIN).

`--oo-schema lego_oo` adds the same rows decomposed by table inheritance into classes (sets by
kind of theme, colours by hue, builders by country); question 19 reads its colour wheel,
`lego_oo.colour_wheel`, so the questions need this flag. `--partitioning` adds partitioned copies,
one schema per method: `lego_inheritance`, `lego_range` and `lego_hash`. The questions read `lego`;
the others give the surveyor classes and partitions to stand on, and you tables for your own
questions.

The generator ends by printing `SUMMARY` lines: each table's size, then the load's
`wall_seconds`.

## 2. Turn the surveyor off

```sh
warren-bench kit surveyor-sql | psql -X -d $DB -v mode=off -f -
warren-bench kit surveyor-sql | psql -X -d $DB -v mode=off -v apply=1 -f -
```

The bench prints the surveyor script and psql reads it from standard input: the first prints what
turning the surveyor off would do, the second does it. Off drops every surveyor and takes the
library out of the database's preload, every other library kept, so that every new session plans
with your indexes and nothing of ours: the answers and times of the next two steps are PostgreSQL
alone's. On a fresh load it finds nothing to drop. Applied, it checks that no surveyor stands and
that new sessions do not preload the library, and ends with the line that begins
`-- every check holds`. Sessions already open keep the libraries they started with.

## 3. Record the answers with PostgreSQL alone

```sh
T="postgres://USER@HOST:5432/lego?options=-c%20search_path%3Dlego%2Cpublic"
warren-bench truth --target lego lego.lego_purchases "$T" --out runs/expected.parquet
```

The bench runs each of the kit's questions (*The questions*, below) twice, each time in a new
session, and keeps each answer's row count and a digest of its rows, whatever their order; the two
must agree. Every later answer is checked against these. The questions name their tables without a
schema, so `search_path` picks them: `lego` here, and `public` for the extensions.
`lego.lego_purchases` is the table a run is recorded by: its size, its partitions and its
statistics.

It logs each question's expected answer, its rows and digest, and writes `runs/expected.parquet`.
It refuses to overwrite that file without `--force`, and stops with exit status 2 at a question
whose two answers differ. Keep the file: a later load made by the same command, with the same
questions, can use it and skip this step.

## 4. Time every question with PostgreSQL alone

```sh
warren-bench run --index-sets all --target alone lego.lego_purchases "$T" \
    --expected runs/expected.parquet --out runs/alone
```

Each question runs three times cold, each in a new session, and five times warm, in the last one,
on your indexes and nothing of ours: the index set `all` turns nothing off. These are the `alone`
figures of step 7. The run prints its summary, and writes it with every repetition under
`runs/alone/`. This step is optional: without it, step 6 still sets your indexes beside the
surveyor.

## 5. Turn the surveyor on

```sh
warren-bench kit surveyor-sql | psql -X -d $DB -v mode=on -f -
warren-bench kit surveyor-sql | psql -X -d $DB -v mode=on -v apply=1 -f -
```

Read the first before you run the second. On creates the extension from the files *Install* put in
the server, makes every new session of the database preload its library beside every library it
preloads already, and gives every table one surveyor on the table's own unique key, its primary
key first, else the one with the fewest columns. Classes, partitions and partitioned tables are
included: a partition takes its partitioned table's key, and a partitioned table's surveyor takes
its partitions' as its own. Then ANALYZE on each table given a surveyor that lies under no other
table given one. Making a surveyor reads no table; the ANALYZE reads its sample of each, as any
ANALYZE does. From here every new session plans with the surveyor.

Applied, it checks that each table it gave a surveyor has one, valid and holding no page, and that
new sessions preload the library, and ends with the line that begins `-- every check holds`. A
table with no unique key, or whose key holds a type the surveyor has no class for, gets none, and
is named.

## 6. Run every question, with the surveyor and without it

```sh
warren-bench run --index-sets dba,all --target lego lego.lego_purchases "$T" \
    --expected runs/expected.parquet --out runs/run-1
```

Each question runs under two index sets, three times cold and five warm on each. Under `all`
nothing is dropped: the surveyor at work. Under `dba`, every repetition is
`BEGIN; DROP INDEX <every surveyor>; SET LOCAL transaction_read_only = on; <question>; ROLLBACK`:
your indexes, with the surveyor's library still loaded, so the respelling still acts. Beside
`alone`, `dba` shows what the respelling does; beside `all`, what the surveyor's measurements do.
`--index-sets all` by itself runs only the surveyor's side.

Every answer is checked against `runs/expected.parquet`; a wrong one is shown as WRONG in place of
its figures, and the exit status is 1. The bench's README lists every exit status.

The run leaves the surveyor on. To put the database back as the load left it:

```sh
warren-bench kit surveyor-sql | psql -X -d $DB -v mode=off -v apply=1 -f -
```

## 7. Read the difference

`runs/run-1/summary.txt` (and `runs/alone/summary.txt`) gives, per question and index set, whether
every answer was right, the pages read from outside shared buffers and found in them, the temp
pages written, then the time. The pages come from each repetition's `EXPLAIN (ANALYZE, BUFFERS)`,
run in a second session; the time is the client's, planning included. `scans.txt` gives each
scan's estimated rows beside its actual rows: that is where the surveyor acts. Every file a run
writes for a program to read is typed Parquet; `warren-bench export` writes any of them as text.
[The bench's README](../warren-bench/README.md) holds every flag and file. To read one plan by
hand, *Checking it* in [the surveyor's README](../warren-surveyor-pg/README.md#checking-it) gives
the order.

Read the pages first. A plan that reads the disk and runs a little faster has lost to one that reads
none: your disk is busy with everything else your server does, and a time taken on a quiet machine
cannot show that.

## The questions

The kit holds 25 questions as plain SQL on the generator's tables, each an aggregate, a top N or an
ordered list. `warren-bench kit write kit` writes them into `kit/questions/`, beside their bound
values and the surveyor script, for you to read; it refuses to write over a file that is there.
Twelve ask one kind of thing:

| | question | what it reads |
|---|---|---|
| 1 | purchases per day in December 2020 | a span of time |
| 2 | two hours of purchases on 15 December 2020, with their sets | a span, through each purchase's collection row |
| 3 | December's purchases in every year | one month of every year |
| 4 | builders in the 8 km box around central London, by postcode district | a box |
| 5 | builders within 50 km of central Aarhus, by city | a distance |
| 6 | parts named with the word "torso", by category | words |
| 7 | sets whose name contains "aeroflot" | trigrams |
| 8 | Gear's sets per year | one theme |
| 9 | the sets of theme 618, with their collection rows | a rare theme |
| 10 | the parts of the category Tools | one category |
| 11 | the sets under Castle, by theme | a theme subtree, by WITH RECURSIVE |
| 12 | the Star Wars class's sets per year | a class, by its own condition |

Twelve put those together (13 to 24), and the last, 25, puts most of them in one: Town's truck sets
with a helmet among their parts, bought in the Decembers of 2016 to 2020 within 15 km of central
London. Every answer has rows, at `small` as at `huge`.

## Our run

Run on 2026-10-05 with the command in *Run it all*, `kit/run.sh --size huge`, from a clone of this
repository, on a PostgreSQL 18.6 server left on its default settings (128 MB of shared buffers,
4 MB of `work_mem`), on a machine with 123 GiB of memory, 16 cores (32 threads) and an NVMe drive.
The huge load, with `--oo-schema` and `--partitioning`, took 36 minutes and made a 197 GB
database: 100.7 million purchases, 80.5 million collection rows, 115.5 million inventory lines,
2.0 million builders and 2.0 million sets. Turning the surveyor on gave 385 tables a surveyor and
named 14 that have none, each for want of a unique key. Each question ran on PostgreSQL alone
(`alone`), then under `dba` and `all`, three times cold and five warm on each: every answer RIGHT,
exit status 0, in 197 seconds for PostgreSQL alone and 347 for the other two.

Each cell is `alone / dba / all`: PostgreSQL alone with the DBA's indexes, then the same indexes
with the surveyor's library loaded and every surveyor dropped (the respelling alone), then the
surveyor on. The table is read from each run's `results/`, exported with `warren-bench export`:
pages are 8 kB, medians over all eight repetitions, where `summary.txt` gives warm medians;
"touched" is the pages found in shared buffers plus those read from outside them. Times are medians
over the five warm repetitions. The tables the questions read come to 51 GB, which the machine's
memory holds, so a page from outside shared buffers may have come from the operating system's cache
rather than the drive; on a server whose memory cannot hold them, it comes from the drive.

| question | rows | answers | pages read from outside shared buffers, median (largest) | pages touched, execution | pages touched, planning | temp pages | planning ms, warm | time ms, warm |
|---|--:|---|--:|--:|--:|--:|--:|--:|
| single/01_purchases_per_day_december_2020 | 31 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 870499 / 870499 / 870499 | 0 / 15 / 324 | 1309 / 1309 / 1309 | 0.10 / 0.17 / 1.42 | 115.01 / 109.43 / 110.21 |
| single/02_two_hours_with_their_sets | 2930 | RIGHT | 0 (10683) / 0 (6840) / 0 (0) | 26513 / 29122 / 29122 | 38 / 83 / 440 | 0 / 0 / 0 | 0.27 / 0.30 / 1.64 | 13.85 / 13.83 / 15.29 |
| single/03_december_per_year | 77 | RIGHT | 60119 (60119) / 60119 (60119) / 60114 (60114) | 8264837 / 8264837 / 8264837 | 0 / 15 / 230 | 24763 / 24763 / 24763 | 0.10 / 0.17 / 1.52 | 1543.75 / 1525.92 / 1528.51 |
| single/04_london_8km_box_by_district | 20 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 47154 / 47154 / 9816 | 30 / 75 / 435 | 0 / 0 / 0 | 0.30 / 0.40 / 3.93 | 24.66 / 24.96 / 25.22 |
| single/05_aarhus_50km_by_city | 1 | RIGHT | 0 (7755) / 0 (6605) / 7303.5 (8460) | 237052 / 237053 / 16614 | 42 / 100 / 458 | 0 / 0 / 0 | 0.37 / 0.47 / 4.08 | 83.23 / 79.71 / 80.67 |
| single/06_torso_parts_by_category | 9 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 127 / 127 / 127 | 5 / 34 / 22 | 0 / 0 / 0 | 0.11 / 0.17 / 0.62 | 0.66 / 0.75 / 1.28 |
| single/07_aeroflot_sets | 76 | RIGHT | 0 (4) / 0 (0) / 0 (0) | 121 / 121 / 121 | 15 / 48 / 87 | 0 / 0 / 0 | 0.15 / 0.22 / 0.72 | 0.59 / 0.68 / 1.19 |
| single/08_gear_sets_per_year | 22 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 367 / 367 / 367 | 0 / 17 / 400 | 0 / 0 / 0 | 0.06 / 0.08 / 3.13 | 2.90 / 2.78 / 5.89 |
| single/09_theme_618_collection_rows | 7 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 170 / 170 / 141 | 18 / 48 / 82 | 0 / 0 / 0 | 0.10 / 0.17 / 0.57 | 1.83 / 2.23 / 0.90 |
| single/10_tools_parts | 8 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 4 / 4 / 4 | 0 / 12 / 14 | 0 / 0 / 0 | 0.03 / 0.05 / 0.45 | 0.09 / 0.10 / 0.60 |
| single/11_castle_subtree_per_theme | 17 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 158 / 158 / 187 | 0 / 33 / 205 | 0 / 0 / 0 | 0.13 / 0.15 / 1.22 | 1.85 / 1.71 / 2.75 |
| single/12_star_wars_class_per_year | 19 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 844 / 844 / 844 | 0 / 17 / 1196 | 0 / 0 / 0 | 0.07 / 0.11 / 7.59 | 9.46 / 9.32 / 16.76 |
| composed/13_decembers_2016_2020_with_builders | 5 | RIGHT | 0 (10633) / 0 (4629) / 0 (0) | 2044472 / 2044472 / 2044472 | 0 / 15 / 229 | 9204 / 9204 / 9204 | 0.12 / 0.17 / 1.34 | 827.08 / 824.06 / 830.11 |
| composed/14_london_15km_december_2020 | 31 | RIGHT | 22819.5 (22837) / 22819.5 (22834) / 22273 (22292) | 887241 / 887241.5 / 887241 | 18 / 48 / 896 | 0 / 0 / 0 | 0.30 / 0.36 / 6.55 | 97.50 / 95.33 / 91.96 |
| composed/15_helmet_accessories | 371 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 123 / 123 / 123 | 1 / 13 / 54 | 0 / 0 / 0 | 0.06 / 0.10 / 0.59 | 0.35 / 0.42 / 0.98 |
| composed/16_town_2010s_per_theme | 22 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 4401 / 4401 / 502 | 14 / 47 / 7435 | 0 / 0 / 0 | 0.21 / 0.30 / 57.06 | 56.45 / 57.37 / 61.15 |
| composed/17_newest_castle_knights | 20 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 8934 / 9006 / 5541 | 1 / 34 / 235 | 0 / 0 / 0 | 0.16 / 0.23 / 1.59 | 8.64 / 11.23 / 37.77 |
| composed/18_star_wars_decembers | 5 | RIGHT | 548348 (548348) / 548348 (548348) / 548345 (548346) | 2577994 / 2577999.5 / 2577994 | 38 / 83 / 1508 | 27200 / 27220 / 27252 | 0.39 / 0.53 / 10.39 | 1576.87 / 1581.24 / 1576.49 |
| composed/19_castle_primary_colours | 17 | RIGHT | 754116 (758064) / 754116 (758064) / 39817.5 (39927) | 759679 / 759681 / 207619 | 38 / 113 / 311 | 126243 / 126234 / 639 | 0.43 / 0.55 / 2.19 | 2647.23 / 2586.16 / 291.42 |
| composed/20_denmark_15_december_per_city | 12 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 43822 / 43822 / 43822 | 60 / 133 / 487 | 682 / 682 / 682 | 0.46 / 0.57 / 2.13 | 35.25 / 33.81 / 32.09 |
| composed/21_aarhus_1km_castle_purchases | 20 | RIGHT | 31439.5 (31467) / 0 (13373) / 0 (2784) | 232570 / 16334 / 17860 | 75 / 135 / 464 | 6444 / 0 / 0 | 0.69 / 0.71 / 2.82 | 203.75 / 18.55 / 16.76 |
| composed/22_1980_1999_lego_own_roots | 39 | RIGHT | 0 (0) / 0 (0) / 0 (0) | 2411 / 2411 / 2417 | 0 / 33 / 4258 | 0 / 0 / 0 | 0.20 / 0.23 / 26.62 | 31.80 / 31.15 / 44.79 |
| composed/23_london_ec_decembers | 15 | RIGHT | 0 (0) / 0 (4600) / 0 (0) | 18570.5 / 18570 / 15787 | 48 / 108 / 371 | 0 / 0 / 0 | 0.37 / 0.45 / 1.76 | 29.48 / 29.19 / 18.49 |
| composed/24_castle_crown_sets | 8 | RIGHT | 771504 (776626) / 771347 (777269) / 38731 (38818) | 778262 / 778262 / 4333057 | 41 / 119 / 361 | 0 / 0 / 0 | 0.47 / 0.55 / 2.28 | 1239.09 / 1217.78 / 1504.44 |
| proof/25_town_trucks_helmet_london | 10 | RIGHT | 86176 (86179) / 86175.5 (86179) / 63598 (63640) | 392868.5 / 392872.5 / 374378 | 134 / 255 / 1600 | 0 / 0 / 0 | 2.11 / 2.29 / 13.34 | 234.84 / 225.69 / 334.76 |

How we read it:

- **The answers.** All 600, 25 questions on three sides eight times each, matched the ones
  PostgreSQL alone gave before the surveyor was on, by row count and digest.
- **The disk.** With the surveyor, 19 read 39,818 pages from outside shared buffers at the median
  where PostgreSQL alone read 754,116; 24 read 38,731 against 771,504; 25 63,598 against 86,176;
  21 none against 31,440. On 3, 14 and 18 both read the same, 22,000 to 548,000 pages, by the same
  plan. On 5 the surveyor's plan read 7,304 where PostgreSQL alone read none: by the disk, a loss.
  Temp pages: 19 wrote 639 against 126,243, and 21 none against 6,444; every other question wrote
  the same on every side, within 52 pages on 18.
- **The plans.** `dba` ran PostgreSQL alone's plan on every question but 2, 17 and 21. On 21 the
  respelling alone took the plan from 232,570 pages touched to 16,334: that win is the
  respelling's. The surveyor's plans touched fewer pages on 4 (9,816 against 47,154), 5 (16,614
  against 237,052), 9, 16, 17, 19 (207,619 against 759,679), 23 and 25; more on 24, 4,333,057
  against 778,262, looking the lines up through an index where PostgreSQL alone scans them; and
  more on 2 (29,122 against 26,513) and 11 (187 against 158).
- **Planning.** With the surveyor, planning took 0.45 to 57.06 ms where PostgreSQL alone took 0.03
  to 2.11, and touched up to 7,435 pages against up to 134: the longest where a theme tree is
  walked while the statement is planned, 16 (57.06 ms) and 22 (26.62 ms).
- **Then time.** The surveyor's side ran faster on 1, 3, 5, 9, 14, 19, 20, 21 and 23, the same on
  18, and slower on the rest. 19 ran in 291.42 ms against 2,647.23, and 21 in 16.76 against
  203.75, the respelling's doing. It lost most on 24, 1,504.44 ms against 1,239.09 (by the disk it
  won, by the time it lost), on 25, 334.76 against 234.84, 11 ms of it planning, and on 17, 37.77
  against 8.64 on fewer pages. Summed over the warm medians, the surveyor's side took 6.63 s,
  PostgreSQL alone 8.79 and `dba` 8.48, and 19 alone is 2.36 s of the difference. The sum weighs
  each question by its own time; read the rows first.

At `small` (3.2 GB, four minutes for all of it), every answer was the same too, no plan with the
surveyor read a page from outside shared buffers at the median, and planning took 0.53 to 2.46 ms
against 0.03 to 0.52.

## Licence

The kit (this directory, and the questions and script inside `warren-bench`) and `warren-bench`
are licensed under either of the Apache License, Version 2.0 or the MIT license, at your option
([LICENSE-APACHE](LICENSE-APACHE), [LICENSE-MIT](LICENSE-MIT)). `warren-surveyor-pg` is under the
GNU General Public License, version 3 or later.
