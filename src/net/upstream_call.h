#ifndef WALLY_NET_UPSTREAM_CALL_H
#define WALLY_NET_UPSTREAM_CALL_H

#include <chrono>
#include <cstddef>
#include <functional>
#include <string>

#include <httplib.h>

#include "net/upstream_pool.h"

/// One upstream POST that watches the reader it is streaming to (wally #81).
///
/// The translators sit between an editor and the model endpoint. When the
/// editor abandons a stream -- Esc in Claude Code, the app quitting -- the
/// only thing the endpoint used to see was a TCP close, and only once the
/// next upstream chunk failed to write; during a long prefill nothing arrives,
/// so nothing failed, and the managed edge in front of the endpoint loses the
/// close anyway (InferenceInfra #440: measured 22-41 s of paid decode after
/// the client was gone). Two things fix that, and this call does both:
///
///   1. It NOTICES the editor leaving without waiting for a chunk: a watch
///      thread polls the server request's `is_connection_closed` -- a peek on
///      the editor's socket, pure syscalls on an fd captured by value, so it
///      is safe from a second thread for exactly as long as the request lives,
///      which the join at the end of `PostWatched` guarantees.
///   2. It captures the response's `x-request-id` the moment the headers
///      arrive, before any body byte (`response_handler`), which is the name
///      the endpoint's cancel route wants. On abandon it hands that id to
///      `on_abandoned` -- once -- and then stops the upstream socket.
///
/// The limit, stated: the endpoint's gateway opens the response only at the
/// FIRST TOKEN, so the id is unknown during prefill. An abandon during prefill
/// therefore keeps the upstream open until the first token arrives (or
/// `id_wait` runs out, or the wrapper is stopping), cancels at that moment --
/// which ends the decode, the long part -- and discards everything after.
/// Cancelling inside prefill needs the gateway to name the request earlier;
/// that is InferenceInfra #440's follow-up, not this file's.
namespace wally::net {

struct WatchedCall {
    std::string path;
    std::string body;
    std::string content_type = "application/json";
    /// Called with each response body chunk, exactly as `httplib::Client::Post`
    /// would; never called once the reader is known to be gone. Returning
    /// false MEANS the reader is gone -- a write to it failed -- and is the
    /// usual way a leave is noticed while tokens are flowing: the next chunk
    /// fails to deliver before the poll below gets its turn. The call treats
    /// it as an abandon (names the cancel, then aborts the request) rather
    /// than as a plain stop that nobody follows up.
    httplib::ContentReceiver receiver;
    /// True when the reader this stream is for has hung up. The server
    /// request's `is_connection_closed`.
    std::function<bool()> reader_gone;
    /// True when the wrapper is shutting down: stop waiting for an id.
    std::function<bool()> stopping;
    /// At most once, the moment the reader is known to be gone AND the request
    /// id is known (or the wait for it ended). `request_id` is empty when the
    /// headers never came; `status` is the upstream status, 0 when unknown;
    /// `during_prefill` says the reader left before the headers arrived (the
    /// id, if any, came later). Runs on the watch thread, inside the response
    /// handler, or inside the receiver, with the call's own lock held: it
    /// must not block (queueing is fine, a network call is not) and must not
    /// touch the sink.
    std::function<void(const std::string& request_id, int status, bool during_prefill)>
        on_abandoned;
    /// How often the reader is checked. The watch wakes early when the call
    /// ends, so this is never added to a call that completes.
    std::chrono::milliseconds poll{100};
    /// After an abandon, how long to keep the upstream open waiting for the
    /// headers that carry the id. A prefill longer than this loses the cancel
    /// (the socket is simply dropped, today's behaviour); the bound exists so
    /// an abandoned request cannot hold a server thread and a pooled
    /// connection forever.
    std::chrono::seconds id_wait{120};
};

struct WatchedResult {
    httplib::Result reply{nullptr, httplib::Error::Unknown};
    /// Whether any response body bytes reached `receiver`. Forbids a retry.
    bool received_any = false;
    /// The reader left before the reply completed. Forbids a retry too: the
    /// stop that ended the call looks like a stale connection to the retry
    /// rule, and re-sending the prompt for a reader that is gone is the exact
    /// waste this call exists to end.
    bool abandoned = false;
    /// The response's x-request-id, or empty if the headers never came.
    std::string request_id;
    /// The upstream status, 0 if the headers never came.
    int status = 0;
};

/// Sends `call` on `lease`'s client, watching the reader throughout. Returns
/// once the reply completed, failed, or was stopped; the lease is the
/// caller's to return or discard (an abandoned or failed call's socket is in
/// no state to reuse).
WatchedResult PostWatched(UpstreamLease& lease, const WatchedCall& call);

}  // namespace wally::net

#endif  // WALLY_NET_UPSTREAM_CALL_H
