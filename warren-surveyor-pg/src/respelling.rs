// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The laws of the respelling this extension installs from warren-speller-pg, on tables that each
//! carry a surveyor. The speller's own laws run in
//! [warren-speller](https://github.com/soulstompp/warren-speller); these are the surveyor's. They
//! run here, in the extension's tests, because pgrx runs a crate's `#[pg_test]`s only in the
//! extension it builds.

pub(crate) mod catalogue;
mod purchase_question;
