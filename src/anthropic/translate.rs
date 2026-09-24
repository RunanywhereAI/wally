//! Anthropic Messages ⇄ OpenAI chat translation, including the streaming state
//! machine (port of src/anthropic/translate.cpp). JSON text goes through
//! crate::io::json so it matches nlohmann's `dump()` byte for byte.
//! Owner: the upstream / shim port.
