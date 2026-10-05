// Copyright (C) 2026 Kenneth Allen Flegal
// SPDX-License-Identifier: GPL-3.0-or-later

//! The laws of the respelling this extension installs from warren-pg-speller: one module for each
//! of its three identities, one for the question that reaches all three, and one for what a
//! respelled statement reads. They run here, in the extension's tests, because pgrx runs a crate's
//! `#[pg_test]`s only in the extension it builds.

pub(crate) mod catalogue;
mod distribute;
mod fold;
mod keep;
mod purchase_question;
mod reads;
