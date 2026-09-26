//! Port of tests/test_wally_unit.cpp. One sub-file per owning port so no two
//! ports edit the same file; every C++ case keeps its name (checked by
//! scripts/test/test-ledger.py).

mod common;

#[path = "test_wally_unit/catalog.rs"]
mod catalog;
#[path = "test_wally_unit/cli.rs"]
mod cli;
#[path = "test_wally_unit/diagnostics.rs"]
mod diagnostics;
#[path = "test_wally_unit/diarize.rs"]
mod diarize;
#[path = "test_wally_unit/harness.rs"]
mod harness;
#[path = "test_wally_unit/image.rs"]
mod image;
#[path = "test_wally_unit/lora.rs"]
mod lora;
#[path = "test_wally_unit/models.rs"]
mod models;
#[path = "test_wally_unit/output.rs"]
mod output;
#[path = "test_wally_unit/paths.rs"]
mod paths;
#[path = "test_wally_unit/run.rs"]
mod run;
#[path = "test_wally_unit/shim.rs"]
mod shim;
