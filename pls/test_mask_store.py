"""Tests for the PII mask store and the privacy rewrite that uses it.

No network, no MLX, no wally binary. The real classifier in pii_model is
never called: classify_pii is replaced with a span finder.
"""

import asyncio
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

from fastapi.testclient import TestClient

import mask_store
import pls
from mask_store import MASK_SAFETY, MaskError, MaskStore, store_for

MAYA = "maya@x.com"
ZOE = "zoe@x.com"
OTHER = "a@b.co"
PROMPT = f"email {MAYA} please"


def span(text, needle, category="private_email"):
    start = text.index(needle)
    return {"start": start, "end": start + len(needle), "category": category}


def completion(content="answer", calls=None):
    message = {"role": "assistant", "content": content}
    if calls:
        message["tool_calls"] = calls
    return {
        "id": "x",
        "created": 0,
        "model": "m",
        "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }


def tool_defs(*names):
    return [{"type": "function", "function": {"name": name}} for name in names]


class FakeResponse:
    def __init__(self, payload, status=200):
        self._payload = payload
        self.status_code = status
        self.content = json.dumps(payload).encode()
        self.text = self.content.decode()
        self.headers = {"content-type": "application/json"}

    def json(self):
        return self._payload

    async def aread(self):
        return self.content

    async def aclose(self):
        pass


def _fake_send(payload):
    async def send(request, stream=False):
        return FakeResponse(payload)

    return send


def rows(store, session_id):
    with store._lock:
        return store._db.execute(
            "SELECT token, value FROM masks WHERE session_id = ?",
            (session_id,),
        ).fetchall()


def count(store, session_id=None):
    sql = "SELECT count(*) FROM masks"
    args = ()
    if session_id is not None:
        sql += " WHERE session_id = ?"
        args = (session_id,)
    with store._lock:
        return store._db.execute(sql, args).fetchone()[0]


class MaskStoreTest(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.store = MaskStore(self.dir / "deepseek.sqlite")

    def test_a_span_becomes_a_token_and_a_row(self):
        masked = self.store.mask("s1", PROMPT, [span(PROMPT, MAYA)])
        self.assertEqual(masked, "email ⟦pii:1⟧ please")
        self.assertEqual(rows(self.store, "s1"), [("⟦pii:1⟧", MAYA)])

    def test_demask_restores_the_original_text(self):
        masked = self.store.mask("s1", PROMPT, [span(PROMPT, MAYA)])
        self.assertEqual(self.store.demask("s1", masked), PROMPT)

    def test_the_same_value_reuses_the_token(self):
        self.store.mask("s1", PROMPT, [span(PROMPT, MAYA)])
        again = f"ping {MAYA}"
        masked = self.store.mask("s1", again, [span(again, MAYA)])
        self.assertIn("⟦pii:1⟧", masked)
        self.assertNotIn("⟦pii:2⟧", masked)
        self.assertEqual(count(self.store), 1)

    def test_a_second_value_gets_the_next_number(self):
        self.store.mask("s1", PROMPT, [span(PROMPT, MAYA)])
        text = f"ping {OTHER}"
        masked = self.store.mask("s1", text, [span(text, OTHER)])
        self.assertIn("⟦pii:2⟧", masked)
        self.assertEqual(rows(self.store, "s1")[1][1], OTHER)

    def test_an_unknown_token_is_left_as_text(self):
        self.store.mask("s1", PROMPT, [span(PROMPT, MAYA)])
        self.assertEqual(self.store.demask("s1", "⟦pii:9⟧"), "⟦pii:9⟧")

    def test_demask_does_not_expand_tokens_inside_a_value(self):
        literal = "⟦pii:2⟧"
        masked = self.store.mask("s1", literal, [span(literal, literal, "secret")])
        self.assertEqual(masked, "⟦pii:1⟧")
        self.assertEqual(self.store.demask("s1", masked), literal)

    def test_overlapping_spans_fail(self):
        start = PROMPT.index(MAYA)
        with self.assertRaises(MaskError):
            self.store.mask("s1", PROMPT, [
                {"start": start, "end": start + 4, "category": "private_email"},
                {"start": start, "end": start + len(MAYA), "category": "private_email"},
            ])
        self.assertEqual(count(self.store), 0)

    def test_a_bad_span_fails(self):
        with self.assertRaises(MaskError):
            self.store.mask("s1", PROMPT, [{"start": 0, "end": 10_000, "category": "x"}])
        self.assertEqual(count(self.store), 0)

    def test_two_harnesses_do_not_demask_each_other(self):
        deepseek = store_for("deepseek", self.dir)
        hermes = store_for("hermes", self.dir)
        self.assertNotEqual(deepseek.path, hermes.path)
        deepseek.mask("s1", PROMPT, [span(PROMPT, MAYA)])
        both = f"{MAYA} and {ZOE}"
        hermes.mask("s1", both, [span(both, MAYA), span(both, ZOE)])
        self.assertEqual(deepseek.demask("s1", "⟦pii:1⟧"), MAYA)
        self.assertEqual(hermes.demask("s1", "⟦pii:2⟧"), ZOE)
        self.assertEqual(deepseek.demask("s1", "⟦pii:2⟧"), "⟦pii:2⟧")

    def test_a_bad_harness_id_is_rejected(self):
        with self.assertRaises(ValueError):
            store_for("../x", self.dir)
        self.assertFalse((self.dir.parent / "x.sqlite").exists())
        outside = list(self.dir.parent.glob("*.sqlite"))
        self.assertEqual(outside, [])

    def test_the_database_file_is_mode_0600(self):
        if sys.platform == "win32":
            self.skipTest("posix mode bits")
        store = store_for("deepseek", self.dir)
        self.assertEqual(os.stat(store.path).st_mode & 0o777, 0o600)

    def test_the_mask_directory_is_mode_0700(self):
        if sys.platform == "win32":
            self.skipTest("posix mode bits")
        directory = self.dir / "masks"
        store_for("deepseek", directory)
        self.assertEqual(os.stat(directory).st_mode & 0o777, 0o700)


class SafetyLineTest(unittest.TestCase):
    def test_the_safety_line_is_the_agreed_sentence(self):
        self.assertEqual(pls.MASK_SAFETY, mask_store.MASK_SAFETY)
        self.assertEqual(
            pls.MASK_SAFETY,
            "Tokens like ⟦pii:1⟧ are personal data kept on this device; "
            "copy them unchanged and they are restored before the subagent runs.",
        )

    def test_the_safety_message_is_inserted_once(self):
        messages = [{"role": "user", "content": "email ⟦pii:1⟧ please"}]
        pls.attach_mask_safety(messages)
        pls.attach_mask_safety(messages)
        safety = [message for message in messages if message.get("content") == pls.MASK_SAFETY]
        self.assertEqual(len(safety), 1)
        self.assertEqual(messages[0], {"role": "system", "content": pls.MASK_SAFETY})


class RouterMaskTest(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.config = self.dir / "config.json"
        self.creds = self.dir / "credentials.json"
        self.creds.write_text(json.dumps({"console_url": "http://cloud", "access_token": "t"}))
        self.config.write_text(json.dumps({
            "mode": "hybrid", "policy": "privacy", "max_turns": 20, "harness": "deepseek",
        }))

        pls.CONFIG_PATH = self.config
        pls.CREDENTIALS_PATH = self.creds
        pls.TASKS_PATH = self.dir / "tasks.json"
        pls.TASKS.clear()
        pls.TASK_TEXT.clear()
        pls.SESSION_NEXT.clear()
        pls.turns.clear()
        pls.PLS_SOCKET = ""
        pls.TOKEN = ""
        pls.privacy_gap_noted = False
        pls.waiting = 0

        self.local_bodies = []
        self.sent = []
        self.decided = []
        self.classified = []
        self.verdict = {"answer": "yes", "p_yes": 0.9}
        self.store = MaskStore(self.dir / "deepseek.sqlite")
        self.client = TestClient(pls.app)

        previous_classify = pls.classify_pii
        previous_store = pls.mask_stores.get("deepseek")
        previous_decide = pls.decide
        previous_creds = pls.cloud_credentials
        previous_post = pls.http.post
        previous_send = pls.http.send
        previous_server = pls.ensure_model_server
        profile = pls.PROFILE.set(pls.PROFILES["deepseek"])

        async def fake_server():
            return 1

        async def fake_post(url, json=None, headers=None):
            self.local_bodies.append(json)
            return FakeResponse(completion("local answer"))

        async def fake_creds():
            return {"console_url": "http://cloud", "access_token": "t"}

        async def fake_decide(policy, text):
            self.decided.append((policy, text))
            return self.verdict

        async def fake_classify(text):
            self.classified.append(text)
            found = []
            for needle in (MAYA, ZOE, OTHER):
                start = 0
                while True:
                    index = text.find(needle, start)
                    if index < 0:
                        break
                    found.append({
                        "start": index,
                        "end": index + len(needle),
                        "category": "private_email",
                    })
                    start = index + len(needle)
            return found

        async def recording_send(request, stream=False):
            self.sent.append(json.loads(request.content))
            return await _fake_send(completion("cloud answer"))(request, stream)

        pls.classify_pii = fake_classify
        pls.mask_stores["deepseek"] = self.store
        pls.decide = fake_decide
        pls.cloud_credentials = fake_creds
        pls.http.post = fake_post
        pls.http.send = recording_send
        pls.ensure_model_server = fake_server

        def restore():
            pls.classify_pii = previous_classify
            if previous_store is None:
                pls.mask_stores.pop("deepseek", None)
            else:
                pls.mask_stores["deepseek"] = previous_store
            pls.decide = previous_decide
            pls.cloud_credentials = previous_creds
            pls.http.post = previous_post
            pls.http.send = previous_send
            pls.ensure_model_server = previous_server
            pls.PROFILE.reset(profile)

        self.addCleanup(restore)

    def post(self, body):
        return self.client.post(
            "/v1/chat/completions",
            json=body,
            headers={"x-session-id": "s1"},
        )

    def main(self, text=PROMPT, tools=("subagent", "bash"), policy="privacy"):
        self.config.write_text(json.dumps({
            "mode": "hybrid", "policy": policy, "max_turns": 20, "harness": "deepseek",
        }))
        body = {
            "model": "x",
            "stream": False,
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": text},
            ],
        }
        if tools is not None:
            body["tools"] = tool_defs(*tools)
        self.sent.clear()
        response = self.post(body)
        self.assertTrue(self.sent, response.text)
        return self.sent[0]

    def user_text(self, body):
        return "\n".join(
            message.get("content") or ""
            for message in body["messages"]
            if message.get("role") == "user"
        )

    def test_a_private_prompt_reaches_the_cloud_masked(self):
        cloud = self.main()
        text = self.user_text(cloud)
        self.assertIn("⟦pii:1⟧", text)
        self.assertIn("pls-task:", text)
        self.assertNotIn(MAYA, text)
        self.assertIn({"role": "system", "content": pls.MASK_SAFETY}, cloud["messages"])
        names = [tool["function"]["name"] for tool in cloud["tools"]]
        self.assertNotIn("bash", names)
        self.assertIn("subagent", names)

    def test_the_decision_model_sees_the_raw_prompt(self):
        self.main()
        self.assertTrue(any(MAYA in text for _policy, text in self.decided))

    def test_a_failed_pii_model_withholds_the_prompt(self):
        async def boom(_text):
            raise RuntimeError("down")

        pls.classify_pii = boom
        cloud = self.main()
        record = next(iter(pls.TASKS.values()))
        self.assertEqual(self.user_text(cloud), pls.placeholder(record))
        self.assertNotIn(MAYA, json.dumps(cloud))
        self.assertNotIn(
            {"role": "system", "content": pls.MASK_SAFETY}, cloud["messages"])

    def test_the_child_sees_the_real_value(self):
        self.main()
        record = next(iter(pls.TASKS.values()))
        self.local_bodies.clear()
        self.post({
            "model": "x",
            "stream": False,
            "messages": [{
                "role": "user",
                "content": f"pls-task: {record['id']}\nemail ⟦pii:1⟧",
            }],
        })
        blob = json.dumps(self.local_bodies[-1])
        self.assertIn(MAYA, blob)
        self.assertNotIn("⟦pii:1⟧", blob)
        self.assertNotIn(pls.MASK_SAFETY, blob)

    def test_cost_does_not_mask(self):
        self.verdict = {"answer": "yes", "p_yes": 0.9}
        cloud = self.main(policy="cost")
        self.assertIn(MAYA, self.user_text(cloud))
        self.assertNotIn(
            {"role": "system", "content": pls.MASK_SAFETY}, cloud["messages"])

    def test_a_privacy_no_verdict_still_gets_the_delegate_line(self):
        # The plan's worked example says "do it yourself". A privacy "no" is
        # still a summary record, so the parent is told to launch the spawn
        # tool. The guard would reject anything else.
        self.verdict = {"answer": "no", "p_yes": 0.1}
        cloud = self.main()
        text = self.user_text(cloud)
        self.assertIn("⟦pii:1⟧", text)
        self.assertNotIn(MAYA, text)
        self.assertIn("Launch the `subagent` tool", text)
        self.assertIn("pls-task:", text)
        self.assertNotIn("do it yourself", text)

    def test_a_privacy_side_call_is_masked(self):
        cloud = self.main(tools=None)
        text = self.user_text(cloud)
        self.assertIn("⟦pii:1⟧", text)
        self.assertNotIn(MAYA, text)

    def test_a_summary_is_masked_on_the_same_session(self):
        self.store.mask("s1", PROMPT, [span(PROMPT, MAYA)])
        text = f"done {MAYA} and {ZOE}"
        masked = asyncio.run(pls.mask_summary(text, "s1"))
        self.assertIn("⟦pii:1⟧", masked)
        self.assertIn("⟦pii:2⟧", masked)
        self.assertNotIn(MAYA, masked)
        self.assertNotIn(ZOE, masked)
        self.assertEqual(self.store.demask("s1", masked), text)

    def test_a_failed_summary_mask_returns_the_failure_sentence(self):
        async def boom(_text):
            raise RuntimeError("down")

        pls.classify_pii = boom
        result = asyncio.run(pls.mask_summary(f"done {MAYA}", "s1"))
        self.assertEqual(result, pls.SUMMARISE_FAILED)
        self.assertNotIn(MAYA, result)

    def test_local_only_does_not_call_the_masker(self):
        pls.TASKS["local"] = {
            "id": "aabbcc-t1",
            "policy": "privacy",
            "private": True,
            "summary": "local_only",
            "decided": True,
            "mask_session": "s1",
        }
        before = len(self.classified)
        response = self.post({
            "model": "Hybrid-Wally-0.1-local",
            "stream": False,
            "messages": [{
                "role": "user",
                "content": f"pls-task: aabbcc-t1\nemail {MAYA} please",
            }],
        })
        self.assertEqual(len(self.classified), before)
        self.assertEqual(
            response.json()["choices"][0]["message"]["content"], pls.WITHHELD)

    def test_a_restart_demasks_from_the_saved_record(self):
        self.main()
        saved = pls.TASKS_PATH.read_text()
        self.assertNotIn(MAYA, saved)
        record = next(iter(pls.TASKS.values()))
        self.assertIn("masked", record)
        self.assertEqual(record["mask_session"], "s1")
        pls.TASKS.clear()
        pls.TASK_TEXT.clear()
        pls.load_tasks()
        loaded = next(iter(pls.TASKS.values()))
        self.assertNotIn(MAYA, json.dumps(loaded))
        self.local_bodies.clear()
        self.post({
            "model": "x",
            "stream": False,
            "messages": [{
                "role": "user",
                "content": f"pls-task: {loaded['id']}\nemail ⟦pii:1⟧",
            }],
        })
        self.assertIn(MAYA, json.dumps(self.local_bodies[-1]))
