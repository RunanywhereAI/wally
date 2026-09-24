//! test_wally_unit.cpp cases owned by the upstream / shim port. Port each C++ case below as a
//! `#[test] fn <same name>()` in this file, then delete its line from this list.
//! Test bodies are in explore-main/tests/test_wally_unit.cpp (grep the name).
//!
//! - [ ] loopback_token
//! - [ ] upstream_failure_mapping
//! - [ ] stream_usage_reports_input_tokens
//! - [ ] estimate_request_tokens
//! - [ ] message_start_usage_carries_input_estimate
//! - [ ] stream_usage_falls_back_when_endpoint_never_reports_it
//! - [ ] reasoning_content_counts_without_an_unsigned_block
//! - [ ] system_turns_fold_into_the_leading_system_message
//! - [ ] unrunnable_web_search_adds_system_note

#[allow(unused_imports)]
use super::common;
