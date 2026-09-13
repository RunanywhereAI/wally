#include "net/upstream_call.h"

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <mutex>
#include <thread>

namespace wally::net {

namespace {

/// Everything the response handler, the receiver and the watch thread share.
/// The mutex is the only thing coordinating them; every field is read and
/// written under it except `abandoned`, which the hot receiver reads per
/// chunk without the lock.
struct WatchState {
    std::mutex mutex;
    std::condition_variable ended;   // signalled when send() returns
    std::atomic<bool> abandoned{false};
    bool done = false;               // send() returned
    bool headers_seen = false;
    bool during_prefill = false;     // the abandon came before the headers
    bool notified = false;           // on_abandoned already fired
    std::string request_id;
    int status = 0;
};

std::string RequestIdOf(const httplib::Response& response) {
    // Header names are case-insensitive; httplib's get_header_value already
    // matches them that way.
    return response.get_header_value("x-request-id");
}

/// Records the abandon. Caller holds the mutex.
void AbandonLocked(WatchState& state) {
    if (state.abandoned.load()) {
        return;
    }
    state.abandoned.store(true);
    state.during_prefill = !state.headers_seen;
}

/// Fires on_abandoned exactly once, for a request that ran something: a
/// refusal (status >= 400) ran nothing and is not worth a cancel, so it is
/// marked as handled without a call. Caller holds the mutex.
void NameLocked(WatchState& state, const WatchedCall& call) {
    if (state.notified) {
        return;
    }
    state.notified = true;
    if (state.headers_seen && state.status >= 400) {
        return;
    }
    if (call.on_abandoned) {
        call.on_abandoned(state.request_id, state.status, state.during_prefill);
    }
}

}  // namespace

WatchedResult PostWatched(UpstreamLease& lease, const WatchedCall& call) {
    WatchedResult result;
    WatchState state;
    httplib::Client& client = lease.client();

    httplib::Request request;
    request.method = "POST";
    request.path = call.path;
    request.body = call.body;
    request.set_header("Content-Type", call.content_type);

    request.response_handler = [&](const httplib::Response& response) {
        std::lock_guard<std::mutex> lock(state.mutex);
        state.headers_seen = true;
        state.request_id = RequestIdOf(response);
        state.status = response.status;
        if (state.abandoned.load()) {
            // The reader left during prefill and the id has just arrived:
            // this is the moment the cancel can be named. Nothing that
            // follows the headers is for anyone, so the request is aborted
            // here (returning false is Error::Canceled to httplib).
            NameLocked(state, call);
            return false;
        }
        return true;
    };
    if (call.receiver) {
        request.content_receiver = [&](const char* data, size_t length, size_t /*offset*/,
                                       size_t /*total*/) {
            if (state.abandoned.load()) {
                // Discard: the sink belongs to a reader that is gone, and the
                // stop() the watch thread issued will end this read shortly.
                return true;
            }
            result.received_any = true;
            if (call.receiver(data, length)) {
                return true;
            }
            // The reader refused the bytes: it is gone, and this is the first
            // anyone here hears of it. The watch gets no further turn -- the
            // false below ends send() -- so the abandon is recorded and the
            // cancel named right here. The headers are in hand by now.
            std::lock_guard<std::mutex> lock(state.mutex);
            AbandonLocked(state);
            NameLocked(state, call);
            return false;
        };
    }
    // With no receiver httplib accumulates the body on the reply, which is
    // what a buffered caller reads; received_any stays false, and the retry
    // rule treats a reply-less failure as "nothing came back" -- true.

    // The watch: notice the reader leaving, name the cancel once the id is
    // known, and stop the upstream. `stop()` is re-issued on every poll after
    // the abandon until send() has returned: a single stop that lands with no
    // request in flight only disconnects, after which httplib would reconnect
    // and send the prompt again. The wait is a condition variable, not a
    // sleep, so a call that completes is not held for the rest of a poll.
    std::thread watch([&] {
        std::chrono::steady_clock::time_point abandoned_at{};
        std::unique_lock<std::mutex> lock(state.mutex);
        while (!state.done) {
            state.ended.wait_for(lock, call.poll, [&] { return state.done; });
            if (state.done) {
                break;
            }
            if (!state.abandoned.load()) {
                // The peek is a syscall on the reader's socket; no need to
                // hold the handler and the receiver up for it.
                lock.unlock();
                const bool gone = call.reader_gone && call.reader_gone();
                lock.lock();
                if (!gone || state.done) {
                    continue;
                }
                AbandonLocked(state);
                abandoned_at = std::chrono::steady_clock::now();
            }
            // Abandoned. Can the cancel be named yet?
            bool stop_now = false;
            const bool waited_out =
                std::chrono::steady_clock::now() - abandoned_at >= call.id_wait;
            const bool stopping = call.stopping && call.stopping();
            if (state.headers_seen) {
                NameLocked(state, call);
                stop_now = true;
            } else if (waited_out || stopping) {
                // No id will come in time. Say so once, then drop the
                // socket: the TCP close is all that is left to send.
                NameLocked(state, call);
                stop_now = true;
            }
            if (stop_now) {
                lock.unlock();
                client.stop();
                lock.lock();
            }
        }
    });

    result.reply = client.send(request);
    {
        std::lock_guard<std::mutex> lock(state.mutex);
        state.done = true;
    }
    state.ended.notify_all();
    watch.join();

    std::lock_guard<std::mutex> lock(state.mutex);
    result.abandoned = state.abandoned.load();
    result.request_id = state.request_id;
    result.status = state.status;
    return result;
}

}  // namespace wally::net
