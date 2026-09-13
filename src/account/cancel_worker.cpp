#include "account/cancel_worker.h"

#include <utility>

namespace wally::account {

CancelWorker::CancelWorker(std::string console_url, Bearer bearer, int timeout_ms,
                           Result on_result, Transport transport)
    : console_url_(std::move(console_url)),
      bearer_(std::move(bearer)),
      timeout_ms_(timeout_ms),
      on_result_(std::move(on_result)),
      client_(std::move(transport)),
      thread_([this] { Run(); }) {}

CancelWorker::~CancelWorker() { Stop(); }

void CancelWorker::Enqueue(const std::string& request_id) {
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (stopping_) {
            return;
        }
        queue_.push_back(request_id);
    }
    wake_.notify_one();
}

int CancelWorker::Stop() {
    int drained = 0;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (stopping_) {
            return 0;
        }
        stopping_ = true;
        drained = static_cast<int>(queue_.size());
    }
    wake_.notify_all();
    if (thread_.joinable()) {
        thread_.join();
    }
    return drained;
}

int CancelWorker::pending() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return static_cast<int>(queue_.size());
}

void CancelWorker::Run() {
    for (;;) {
        std::string request_id;
        {
            std::unique_lock<std::mutex> lock(mutex_);
            wake_.wait(lock, [this] { return stopping_ || !queue_.empty(); });
            if (queue_.empty()) {
                return;  // stopping, and nothing left to send
            }
            request_id = std::move(queue_.front());
            queue_.pop_front();
        }
        std::string error;
        const std::string bearer = bearer_ ? bearer_() : std::string();
        const CancelOutcome outcome =
            client_.CancelRequest(console_url_, bearer, request_id, timeout_ms_, &error);
        if (on_result_) {
            on_result_(request_id, outcome, error);
        }
    }
}

}  // namespace wally::account
