//! Minimal blocking HTTP/1.1 client and server used by the upstream pool and
//! the Anthropic shim (replaces cpp-httplib). Owner: the upstream / shim port.
//! Must handle Content-Length and chunked bodies both ways, keep-alive with
//! several requests per connection, `Expect: 100-continue`, HEAD, close-delimited
//! bodies, abrupt disconnects; https via native-tls (OS trust store).
