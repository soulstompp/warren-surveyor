# warren-surveyor

`warren-surveyor` is a collection of Rust crates which together enhance the Postgres query planning process.

- [`warren-surveyor-pg`](warren-surveyor-pg/README.md) is a query preprocessor which works with indexes to reduce guesswork and costly scans. It is implemented as a PostgreSQL 18 extension.

- [`warren-pg-speller`](warren-pg-speller/README.md) examines queries and where necessary rewrites them to a form digestible by the planner.
- [`warren-bench`](warren-bench/README.md) implements an execution environment consisting of a sample database and benchmark queries.

## Requirements

PostgreSQL 18, Rust 1.96 or later and cargo-pgrx 0.19.2.

### Prerequisites

Install prerequisites. On MacOS:

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
cargo install --locked cargo-pgrx
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

### Postgres.app on MacOS

Do not use the app-bundled [Postgres.app](https://postgresapp.com/). `cargo pgrx install` will attempt to install a control file to its /Application location which MacOS forbids.

## kit

[kit/run.sh](kit/run.sh) is a shell script which does an end-to-end installation and run. See its [README](kit/README.md).

## Author

Copyright (C) 2026 Kenneth Allen Flegal

## License

`warren-surveyor-pg` is licensed under the GNU General Public License, version 3 or later
([LICENSE](LICENSE)). The speller, the bench and the kit are licensed under either of the Apache
License, Version 2.0 or the MIT license, at your option; each holds both texts.
