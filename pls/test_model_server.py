"""Model server tests.

The fake tests replace the prompt builder and tokenizer, so no model loads.
The real test loads the model named by PLS_TEST_MODEL and is skipped without it:

    PLS_TEST_MODEL=/path/to/mlx-model python -m unittest test_model_server
"""

import os
import unittest

REAL_MODEL = os.environ.get("PLS_TEST_MODEL")
os.environ.setdefault("PLS_MODEL", REAL_MODEL or "unused-by-fake-tests")

from fastapi.testclient import TestClient

import model_server


class FakeTokenizer:
    def encode(self, text):
        return text.split()


class ReplyBudgetTest(unittest.TestCase):
    def test_the_reply_is_shortened_to_fit_the_context(self):
        self.assertEqual(model_server.reply_budget(900, 500, 1000), 100)


class ContextCapTest(unittest.TestCase):
    def setUp(self):
        self.saved = (model_server.processor, model_server.build_prompt, model_server.MAX_KV)
        model_server.processor = FakeTokenizer()
        model_server.build_prompt = lambda messages, tools: " ".join(m["content"] for m in messages)
        model_server.MAX_KV = 10

    def tearDown(self):
        model_server.processor, model_server.build_prompt, model_server.MAX_KV = self.saved

    def test_a_prompt_past_the_context_is_refused_before_generation(self):
        response = TestClient(model_server.app).post(
            "/v1/chat/completions",
            json={"messages": [{"role": "user", "content": "word " * 20}]},
        )
        self.assertEqual(response.status_code, 400)
        self.assertEqual(response.json()["error"]["code"], "context_length_exceeded")


@unittest.skipUnless(REAL_MODEL, "set PLS_TEST_MODEL to an MLX model directory")
class PrefixCacheTest(unittest.TestCase):
    def test_a_follow_up_turn_reuses_the_cached_prefix(self):
        history = [
            {"role": "system", "content": "You are a coding agent. " + "Rules follow. " * 1500},
            {"role": "user", "content": "Say ok."},
        ]
        with TestClient(model_server.app) as client:
            first = client.post("/v1/chat/completions",
                                json={"messages": history, "max_tokens": 4}).json()
            follow_up = [*history, first["choices"][0]["message"],
                         {"role": "user", "content": "Say ok again."}]
            second = client.post("/v1/chat/completions",
                                 json={"messages": follow_up, "max_tokens": 4}).json()
        self.assertGreater(second["usage"]["cached_tokens"], 2000)

    def test_a_context_stays_cached_while_two_others_run(self):
        # Three 12k-token subagent contexts: past APC's default two checkpoints
        # and 32k-token pool, which made the first one re-prefill.
        def context(name):
            return [{"role": "system", "content": f"Agent {name}. " + f"{name} rule. " * 4000},
                    {"role": "user", "content": "Say ok."}]

        with TestClient(model_server.app) as client:
            replies = {}
            for name in ("alpha", "beta", "gamma"):
                replies[name] = client.post("/v1/chat/completions", json={
                    "messages": context(name), "max_tokens": 4}).json()
            follow_up = [*context("alpha"), replies["alpha"]["choices"][0]["message"],
                         {"role": "user", "content": "Say ok again."}]
            again = client.post("/v1/chat/completions",
                                json={"messages": follow_up, "max_tokens": 4}).json()
        self.assertGreaterEqual(again["usage"]["cached_tokens"], 8192)


if __name__ == "__main__":
    unittest.main()
