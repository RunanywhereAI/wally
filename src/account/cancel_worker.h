#ifndef WALLY_ACCOUNT_CANCEL_WORKER_H
#define WALLY_ACCOUNT_CANCEL_WORKER_H

#include <condition_variable>
#include <deque>
#include <functional>
#include <mutex>
#include <string>
#include <thread>

#include "account/console.h"

namespace wally::account {

/// Sends cancels to the control plane off the request path (wally #81).
///
/// A translator learns that the editor abandoned a stream on a thread that
/// must not block -- the watch thread inside the upstream call, or the
/// upstream's response handler -- so the cancel itself is queued here and
/// sent by one worker thread, in order, each bounded by `timeout_ms`. One
/// worker, not one thread per abandon: a session that abandons a hundred
/// streams costs one thread, and `Stop()` has one thing to join.
///
/// Every outcome is reported to `on_result` (the translators write it to
/// their log -- never the editor's terminal, which the tool owns). The bearer
/// comes from `bearer` at SEND time, not construction: the JetBrains proxy
/// renews its token mid-session, and a cancel carrying the old one would be
/// refused. It is the session's own key and is never logged.
class CancelWorker {
   public:
    using Result = std::function<void(const std::string& request_id, CancelOutcome outcome,
                                      const std::string& error)>;
    using Bearer = std::function<std::string()>;

    CancelWorker(std::string console_url, Bearer bearer, int timeout_ms, Result on_result,
                 Transport transport = {});
    ~CancelWorker();
    CancelWorker(const CancelWorker&) = delete;
    CancelWorker& operator=(const CancelWorker&) = delete;

    /// Queue one cancel. Returns at once.
    void Enqueue(const std::string& request_id);

    /// Send everything queued, then stop the worker. Bounded by the queue
    /// depth times `timeout_ms`; typically one call. Returns how many cancels
    /// were sent while stopping, so the caller can say so on stderr.
    int Stop();

    int pending() const;

   private:
    void Run();

    const std::string console_url_;
    const Bearer bearer_;
    const int timeout_ms_;
    const Result on_result_;
    ConsoleClient client_;

    mutable std::mutex mutex_;
    std::condition_variable wake_;
    std::deque<std::string> queue_;
    bool stopping_ = false;
    std::thread thread_;
};

}  // namespace wally::account

#endif  // WALLY_ACCOUNT_CANCEL_WORKER_H
