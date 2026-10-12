# warren-surveyor

`warren-surveyor` is a collection of Rust crates which together enhance the way PostgreSQL plans
queries.

- [`warren-surveyor-pg`](warren-surveyor-pg/README.md) is the surveyor, an index that measures
  without digging: while a statement is planned, it reads your indexes to reduce guesswork and
  costly scans. It is implemented as a PostgreSQL 14 to 18 extension.

- [`warren-pg-speller`](warren-pg-speller/README.md) is the speller. It examines queries while
  PostgreSQL plans them and, where the query and the catalog show that another spelling returns
  the same rows, has the planner plan that respelling instead. The composed file of
  [sql-composer](https://github.com/soulstompp/sql-composer) arrives as plain SQL, and the speller
  reads its statements as it reads any other.
- [`warren-bench`](warren-bench/README.md) runs benchmark questions against a database under sets
  of its indexes, every answer checked. The kit's database is
  [sqlc-brickgen](https://github.com/soulstompp/sql-composer/tree/HEAD/examples/sqlc-brickgen)'s: a
  catalogue of bricks and sets built from a real one to trip indexes, with the DBA's indexes.

## Requirements

PostgreSQL 14 to 18 (14.19, 15.14, 16.10, 17.6 or later), Rust 1.96 or later and cargo-pgrx 0.19.2.
On 14 and 15, building a surveyor warns that an UPDATE changing one of its columns is no longer HOT.

### Prerequisites

Install prerequisites. On macOS:

```sh
brew install postgres # if you don't have it
brew install pkg-config
export PKG_CONFIG_PATH="$(brew --prefix icu4c)/lib/pkgconfig:$PKG_CONFIG_PATH"
brew install icu4c
```

On Debian-type Linux:

```sh
sudo apt update
sudo apt install postgresql postgresql-contrib pkg-config libicu-dev
```

Install pgrx:

```sh
cargo install --locked cargo-pgrx --version 0.19.2
```

## Installation

Install warren-bench:

```sh
cargo install --locked warren-bench --root ~/.local
```

Find out where pg_config is:

```sh
PG_CONFIG=$(which pg_config)
```

Build and install `warren-surveyor-pg`. From the repository base directory:

```sh
cd warren-surveyor-pg && cargo pgrx install --release --pg-config "$PG_CONFIG"
```

This is a safe operation which does not affect existing databases or settings.

### Postgres.app on macOS

Do not use the app-bundled [Postgres.app](https://postgresapp.com/). `cargo pgrx install` will
attempt to install a control file to its `/Applications` location, which macOS forbids.

## kit

[kit/run.sh](kit/run.sh) is a shell script which does an end-to-end installation and run. See its
[README](kit/README.md).

## Author

Copyright (C) 2026 Kenneth Allen Flegal

## License

`warren-surveyor-pg` is licensed under the GNU General Public License, version 3 or later
([LICENSE](LICENSE)). The speller, the bench and the kit are licensed under either of the Apache
License, Version 2.0 or the MIT license, at your option; each holds both texts.
