"""Router tests that never touch the model or the network.

The model server, the cloud and the decision model are all replaced at the
module boundary, so these run in a second.
"""

import asyncio
import contextlib
import hashlib
import io
import itertools
import json
import os
import re
import socket as socket_mod
import stat
import tempfile
import threading
import unittest
from http import HTTPStatus
from pathlib import Path

os.environ.setdefault("WALLY_PROFILE_DIR", tempfile.mkdtemp())

import httpx
from fastapi.testclient import TestClient

import pls
import mask_store
import pii_model

REAL_DECIDE = pls.decide
REAL_CLOUD_CREDENTIALS = pls.cloud_credentials
FIXTURE = Path(__file__).parent / "fixtures" / "dsh_first_turn.json"
PRIVATE_TEXT = "refactor the billing module in src/billing.rs, key sk-live-123"
SEED_IDS = itertools.count(9001)
LOCAL_MODEL_ID = "Hybrid-Wally-0.1-local"
NO_SUBAGENTS_LINE = ("You are a subagent running on this device. You cannot launch "
                     "subagents or message other agents; do the whole task yourself.")


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


def tool_call(name, arguments="{}"):
    return {"id": "c", "type": "function", "function": {"name": name, "arguments": arguments}}


def tool_defs(*names):
    return [{"type": "function", "function": {"name": name}} for name in names]


def tool_names(body):
    return [tool["function"]["name"] for tool in body.get("tools") or []]


def dsh_first_turn(prompt=None):
    """A real dsh 0.1.7 first turn: message 1 is the prompt, 2-4 are injected."""
    body = json.loads(FIXTURE.read_text())
    body.pop("_source", None)
    if prompt is not None:
        body["messages"][1]["content"] = prompt
    body["tools"] = tool_defs("bash", "read", "subagent")
    return body


DSH_PROMPT = dsh_first_turn()["messages"][1]["content"]


def key_of(text, session=None):
    """The task key of a one-prompt turn. A body whose first prompt is `text`
    is its own session unless `session` names the first prompt."""
    first = pls.prompt_key(session if session is not None else text)[:6]
    return f"{first}:{pls.prompt_key(text)}"


class RouterTest(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.config = self.dir / "config.json"
        self.creds = self.dir / "credentials.json"
        self.creds.write_text(json.dumps({"console_url": "http://cloud", "access_token": "t"}))
        self.configure()

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
        pls.local_sem = asyncio.Semaphore(1)

        self.local_bodies = []
        self.decided = []
        self.verdict = {"answer": "no", "p_yes": 0.1}
        self.client = TestClient(pls.app)

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

        pls.ensure_model_server = fake_server
        pls.http.post = fake_post
        pls.cloud_credentials = fake_creds
        pls.decide = fake_decide

        async def no_spans(text):
            return []

        pls.classify_pii = no_spans
        pls.mask_stores.clear()
        pls.mask_stores["deepseek"] = mask_store.MaskStore(self.dir / "deepseek.sqlite")
        self.addCleanup(setattr, pls, "classify_pii", pii_model.classify)
        self.addCleanup(pls.mask_stores.clear)

    # -- helpers -----------------------------------------------------------

    def configure(self, mode="hybrid", policy="privacy", max_turns=20):
        self.config.write_text(json.dumps({"mode": mode, "policy": policy, "max_turns": max_turns}))

    def say(self, answer):
        self.verdict = {"answer": answer, "p_yes": 0.9 if answer == "yes" else 0.1}

    def post(self, path, body=None, headers=None):
        return self.client.post(
            path,
            json=body or {"model": "x", "messages": [{"role": "user", "content": "hi"}]},
            headers=headers or {"x-session-id": "s"},
        )

    def cloud_post(self, body):
        return self.post("/v1/chat/completions", body)

    def record(self, text):
        return pls.TASKS[key_of(text)]

    def seed(self, text, *, policy="privacy", private=False, big=False,
             summary="summary", decided=True):
        """A task record as if the prompt had been decided already."""
        record = {"id": f"t{next(SEED_IDS)}", "policy": policy, "private": private,
                  "big": big, "summary": summary, "decided": decided}
        pls.TASKS[key_of(text)] = record
        pls.TASK_TEXT[record["id"]] = text
        return record

    def applied(self, body, policy="privacy"):
        """What apply_policy produced: its return value, or `body` if it edits in place."""
        result = pls.apply_policy(body, policy)
        return body if result is None else result

    def later(self, body, result="done"):
        """The same conversation after the parent called a tool and got a result."""
        return {**body, "messages": [*body["messages"],
                {"role": "assistant", "content": None, "tool_calls": [tool_call("subagent")]},
                {"role": "tool", "tool_call_id": "c", "content": result}]}

    def child_messages(self, record, extra=""):
        marker = f"pls-task: {record['id']}"
        return [{"role": "system", "content": "s"},
                {"role": "user", "content": marker + ("\n" + extra if extra else "")}]

    def post_child(self, record, extra=""):
        return self.post("/v1/chat/completions",
                         {"model": "x", "messages": self.child_messages(record, extra)})

    def dsh_child(self, record, extra=""):
        """A dsh subagent's first request: the dsh shape, the prompt a task marker."""
        body = dsh_first_turn(f"pls-task: {record['id']}" + ("\n" + extra if extra else ""))
        body["tools"] = tool_defs("bash", "read", "subagent", "subagent_fork", "send_message")
        body["stream"] = False
        return body

    def post_dsh_child(self, record, extra=""):
        return self.post("/v1/chat/completions", self.dsh_child(record, extra))

    def capture_log(self):
        events = []
        original = pls.log
        pls.log = events.append
        self.addCleanup(setattr, pls, "log", original)
        return events

    def model_server_down(self):
        async def cannot_start():
            raise RuntimeError("the model server did not start")

        pls.ensure_model_server = cannot_start

    def answer(self, response):
        return response.json()["choices"][0]["message"]["content"]

    def socket(self):
        """A real unix socket answering like wally: POST /decide and GET /token,
        404 for anything else. Returns (path, request lines seen)."""
        path = str(self.dir / "pls.sock")
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
        server = socket_mod.socket(socket_mod.AF_UNIX, socket_mod.SOCK_STREAM)
        server.bind(path)
        server.listen(4)
        seen = []

        def serve():
            while True:
                try:
                    conn, _ = server.accept()
                except OSError:
                    return
                data = b""
                while b"\r\n\r\n" not in data:
                    chunk = conn.recv(4096)
                    if not chunk:
                        break
                    data += chunk
                head, _, rest = data.partition(b"\r\n\r\n")
                length = re.search(rb"(?i)content-length:\s*(\d+)", head)
                while length and len(rest) < int(length.group(1)):
                    rest += conn.recv(4096)
                line = head.split(b"\r\n")[0].decode()
                seen.append(line)
                if line.startswith("POST /decide "):
                    status, payload = "200 OK", {"answer": "yes", "p_yes": 0.9, "model": "m", "ms": 1}
                elif line.startswith("GET /token"):
                    status, payload = "200 OK", {"access_token": "access-from-wally",
                                                 "console_url": "http://cloud"}
                else:
                    status, payload = "404 Not Found", {"error": "not found"}
                body = json.dumps(payload).encode()
                conn.sendall(f"HTTP/1.1 {status}\r\ncontent-length: {len(body)}\r\n"
                             "connection: close\r\n\r\n".encode() + body)
                conn.close()

        threading.Thread(target=serve, daemon=True).start()
        return path, seen

    def decide_socket(self, status=200, payload=None, raw=None):
        """A unix socket answering POST /decide; returns (path, requests seen)."""
        path = str(self.dir / "decide.sock")
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
        server = socket_mod.socket(socket_mod.AF_UNIX, socket_mod.SOCK_STREAM)
        server.bind(path)
        server.listen(4)
        seen = []

        def serve():
            while True:
                try:
                    conn, _ = server.accept()
                except OSError:
                    return
                data = b""
                while b"\r\n\r\n" not in data:
                    chunk = conn.recv(4096)
                    if not chunk:
                        break
                    data += chunk
                head, _, rest = data.partition(b"\r\n\r\n")
                length = re.search(rb"(?i)content-length:\s*(\d+)", head)
                while length and len(rest) < int(length.group(1)):
                    rest += conn.recv(4096)
                seen.append((head.split(b"\r\n")[0].decode(), rest))
                body = raw if raw is not None else json.dumps(payload).encode()
                phrase = HTTPStatus(status).phrase
                conn.sendall(f"HTTP/1.1 {status} {phrase}\r\ncontent-length: {len(body)}\r\n"
                             "connection: close\r\n\r\n".encode() + body)
                conn.close()

        threading.Thread(target=serve, daemon=True).start()
        return path, seen

    # -- the local lane ----------------------------------------------------

    def test_local_lane_returns_the_model_answer(self):
        record = self.seed("a prompt", summary="summary")
        response = self.post_child(record)
        self.assertEqual(response.status_code, 200)
        self.assertEqual(self.answer(response), "local answer")

    def test_a_private_task_returns_the_answer_with_values_masked(self):
        record = self.seed(PRIVATE_TEXT, private=True, summary="local_only")
        self.assertEqual(self.answer(self.post_child(record)), "local answer")

    def test_tool_calls_pass_through_a_withheld_task(self):
        calls = [tool_call("f")]

        async def with_calls(url, json=None, headers=None):
            return FakeResponse(completion(None, calls))

        pls.http.post = with_calls
        record = self.seed(PRIVATE_TEXT, private=True, summary="local_only")
        message = self.post_child(record).json()["choices"][0]["message"]
        self.assertEqual(message["tool_calls"], calls)
        self.assertIsNone(message["content"])

    def test_cap_appends_wrap_up_and_disables_tools(self):
        self.configure(max_turns=1)
        self.post_child(self.seed("a prompt"))
        sent = self.local_bodies[-1]
        self.assertEqual(sent["tool_choice"], "none")
        self.assertEqual(sent["messages"][-1]["content"], pls.WRAP_UP)

    def test_a_summarised_answer_is_shortened_by_the_local_model(self):
        async def long_then_short(url, json=None, headers=None):
            if pls.SUMMARY_PROMPT in json["messages"][0]["content"]:
                return FakeResponse(completion("short summary"))
            return FakeResponse(completion("x" * 500))

        pls.http.post = long_then_short
        self.assertEqual(self.answer(self.post_child(self.seed("a prompt"))), "short summary")

    def test_summary_redacts_the_values_it_keeps(self):
        blurb = "Row 1 Maya Thornton maya@x.com (555) 010-0101 1988-04-12 " + "pad " * 90
        pls.http.post = _fake_return(completion(blurb))
        content = self.answer(self.post_child(self.seed("a prompt")))
        self.assertNotIn("maya@x.com", content)
        self.assertIn("[email]", content)

    def test_home_is_shortened_in_a_summary(self):
        pls.http.post = _fake_return(completion(str(Path.home()) + "/w/a.csv ok"))
        self.assertIn("~/w/a.csv", self.answer(self.post_child(self.seed("a prompt"))))

    def test_a_summary_that_fails_says_so_instead_of_leaking_the_result(self):
        async def summary_fails(url, json=None, headers=None):
            if pls.SUMMARY_PROMPT in json["messages"][0]["content"]:
                raise RuntimeError("no model")
            return FakeResponse(completion("x" * 500))

        pls.http.post = summary_fails
        self.assertEqual(self.answer(self.post_child(self.seed("a prompt"))), pls.SUMMARISE_FAILED)

    # -- the child's task: marker, real text, summary type -----------------

    def test_a_private_child_gets_the_real_text_in_place_of_the_marker(self):
        record = self.seed(PRIVATE_TEXT, private=True, summary="local_only")
        self.post_child(record)
        users = [m for m in self.local_bodies[-1]["messages"] if m["role"] == "user"]
        self.assertEqual(users[0]["content"], PRIVATE_TEXT)

    def test_a_private_child_is_told_nothing_leaves_the_device(self):
        record = self.seed(PRIVATE_TEXT, private=True, summary="local_only")
        self.post_child(record)
        system = self.local_bodies[-1]["messages"][0]["content"]
        self.assertIn(pls.CHILD_RULES["local_only"], system)

    def test_a_summary_child_is_told_to_end_with_a_summary(self):
        self.post_child(self.seed("a prompt"))
        system = self.local_bodies[-1]["messages"][0]["content"]
        self.assertIn(pls.CHILD_RULES["summary"], system)
        self.assertNotIn(pls.CHILD_RULES["local_only"], system)

    def test_the_marker_is_stripped_from_the_child_prompt(self):
        record = self.seed("a prompt")
        self.post_child(record, "list the files")
        sent = self.local_bodies[-1]["messages"]
        self.assertEqual(next(m for m in sent if m["role"] == "user")["content"], "list the files")
        self.assertNotIn("pls-task:", json.dumps(sent))

    def test_a_child_with_an_unknown_id_returns_its_answer_under_privacy(self):
        record = {"id": "t424242"}
        self.assertEqual(self.answer(self.post_child(record)), "local answer")

    def test_a_child_with_an_unknown_id_is_summarised_under_cost(self):
        self.configure(policy="cost")
        self.assertEqual(self.answer(self.post_child({"id": "t424242"})), "local answer")
        system = self.local_bodies[-1]["messages"][0]["content"]
        self.assertIn(pls.CHILD_RULES["summary"], system)

    def test_resolve_child_reports_local_only_for_a_private_task(self):
        record = self.seed(PRIVATE_TEXT, private=True, summary="local_only")
        kind, messages = pls.resolve_child(self.child_messages(record)[1:], "privacy")
        self.assertEqual(kind, "local_only")
        self.assertEqual(messages[0]["content"], PRIVATE_TEXT)

    def test_resolve_child_reports_summary_and_strips_the_marker(self):
        record = self.seed("a prompt")
        kind, messages = pls.resolve_child(self.child_messages(record, "list the files")[1:], "privacy")
        self.assertEqual(kind, "summary")
        self.assertEqual(messages[0]["content"], "list the files")

    def test_resolve_child_defaults_to_local_only_under_privacy_and_summary_under_cost(self):
        messages = [{"role": "user", "content": "pls-task: t424242\ngo"}]
        self.assertEqual(pls.resolve_child(messages, "privacy")[0], "local_only")
        self.assertEqual(pls.resolve_child(messages, "cost")[0], "summary")

    def test_resolve_child_without_a_marker_falls_back_like_an_unknown_id(self):
        messages = [{"role": "user", "content": "just do it"}]
        self.assertEqual(pls.resolve_child(messages, "privacy")[0], "local_only")
        self.assertEqual(pls.resolve_child(messages, "cost")[0], "summary")

    def test_resolve_child_ignores_a_marker_that_is_not_at_the_start(self):
        record = self.seed("a prompt")  # summary, so a match would not be local_only
        messages = [{"role": "user", "content": f"see pls-task: {record['id']}"}]
        self.assertEqual(pls.resolve_child(messages, "privacy")[0], "local_only")

    def test_resolve_child_reads_the_marker_from_the_first_real_user_message(self):
        record = self.seed("a prompt")
        messages = [{"role": "user", "content": "<system-reminder>\nrules\n</system-reminder>"},
                    {"role": "user", "content": f"pls-task: {record['id']}\ngo"}]
        self.assertEqual(pls.resolve_child(messages, "privacy")[0], "summary")

    def test_resolve_child_ignores_a_marker_after_the_first_reply(self):
        record = self.seed("a prompt")
        messages = [{"role": "user", "content": "go"},
                    {"role": "assistant", "content": "ok"},
                    {"role": "user", "content": f"pls-task: {record['id']}"}]
        self.assertEqual(pls.resolve_child(messages, "privacy")[0], "local_only")

    def test_a_request_for_the_local_model_id_is_a_subagent(self):
        body = {**dsh_first_turn(), "model": pls.LOCAL_MODEL_ID}
        self.assertEqual(pls.classify(body), "subagent")

    def test_a_marker_with_dsh_note_glued_on_is_found(self):
        record = self.seed("list the files", private=True, summary="local_only")
        note = ('Your parent agent id is "session-9a015f3f". Before you finish, send your '
                'result to that agent with send_message({ agent_id: "session-9a015f3f" }).')
        messages = [{"role": "user", "content": f"pls-task: {record['id']}{note}"}]
        summary, updated = pls.resolve_child(messages, "privacy")
        self.assertEqual(summary, "local_only")
        self.assertEqual(updated[0]["content"], "list the files")

    def test_dsh_note_is_cut_from_a_child_prompt(self):
        record = self.seed("count rows", private=False)
        note = ('Your parent agent id is "s-1". Before you finish, send your result '
                'to that agent with send_message.')
        messages = [{"role": "user", "content": f"pls-task: {record['id']}\ncount rows.{note}"}]
        _, updated = pls.resolve_child(messages, "privacy")
        self.assertEqual(updated[0]["content"], "count rows.")

    def test_a_longer_id_does_not_match_a_shorter_one(self):
        self.assertEqual(pls.TASK_RE.match("pls-task: abc123-t12x").group(1), "abc123-t12")

    def test_the_child_rules_cover_summary_and_local_only(self):
        self.assertLessEqual({"summary", "local_only"}, set(pls.CHILD_RULES))

    # -- modes and config --------------------------------------

    def test_cloud_mode_forces_the_cloud_lane(self):
        self.configure(mode="cloud")
        pls.http.send = _fake_send(completion("cloud answer"))
        response = self.post_child(self.seed("a prompt"))
        self.assertEqual(self.answer(response), "cloud answer")
        self.assertEqual(self.local_bodies, [])

    def test_cloud_mode_forwards_the_body_untouched(self):
        self.configure(mode="cloud")
        sent = []
        pls.http.send = _fake_send_sse(sent, None)
        body = {"stream": True, "tools": tool_defs("bash", "read", "subagent"),
                "messages": [{"role": "system", "content": "s"},
                             {"role": "user", "content": "fix the build"}]}
        self.cloud_post(body)
        self.assertEqual(sent, [body])
        self.assertEqual(self.decided, [])

    def test_models_lists_the_parent_and_subagent_ids(self):
        data = self.client.get("/v1/models").json()["data"]
        self.assertEqual([m["id"] for m in data], [pls.MODEL_ID, LOCAL_MODEL_ID])

    def test_the_parent_model_id_is_hybrid_wally(self):
        self.assertEqual(pls.MODEL_ID, "Hybrid-Wally-0.1")

    # -- one endpoint: classifying a request -------------------------------

    def test_a_body_whose_first_prompt_starts_with_a_task_marker_is_a_subagent(self):
        self.assertEqual(pls.classify(dsh_first_turn("pls-task: t1\nlist the files")), "subagent")

    def test_a_marker_with_an_id_the_router_does_not_know_is_still_a_subagent(self):
        self.assertEqual(pls.classify(dsh_first_turn("pls-task: t424242\ngo")), "subagent")

    def test_the_dsh_first_turn_is_main_chat(self):
        self.assertEqual(pls.classify(dsh_first_turn()), "main")

    def test_a_body_with_no_tools_is_a_side_call(self):
        self.assertEqual(pls.classify({"model": "x", "messages": [
            {"role": "system", "content": "Create a concise title"},
            {"role": "user", "content": "Generate the session title"},
        ]}), "side")

    def test_a_marker_that_is_not_at_the_start_of_the_prompt_is_main_chat(self):
        self.assertEqual(pls.classify(dsh_first_turn("see pls-task: t1")), "main")

    def test_a_marker_in_a_later_user_message_is_not_a_subagent(self):
        body = self.later(dsh_first_turn())
        body["messages"].append({"role": "user", "content": "pls-task: t1"})
        self.assertEqual(pls.classify(body), "main")

    # -- one endpoint: a subagent goes local --------------------------------

    def test_a_subagent_request_is_never_decided(self):
        self.post_dsh_child(self.seed("a prompt"))
        self.assertEqual(self.decided, [])

    def test_a_subagent_request_is_never_decided_under_cost(self):
        self.configure(policy="cost")
        self.post_dsh_child(self.seed("a prompt", policy="cost"))
        self.assertEqual(self.decided, [])

    def test_a_subagent_request_reaches_the_local_model_server_not_the_cloud(self):
        sent = []
        pls.http.send = _fake_send_sse(sent, None)
        response = self.post_dsh_child(self.seed("a prompt"))
        self.assertEqual(self.answer(response), "local answer")
        self.assertEqual(len(self.local_bodies), 1)
        self.assertEqual(sent, [])

    def test_an_unknown_task_id_under_privacy_is_local_only_and_stays_local(self):
        sent = []
        pls.http.send = _fake_send_sse(sent, None)
        response = self.post_dsh_child({"id": "t424242"})
        self.assertEqual(sent, [])
        self.assertEqual(self.answer(response), "local answer")
        system = self.local_bodies[-1]["messages"][0]["content"]
        self.assertIn(pls.CHILD_RULES["local_only"], system)

    def test_the_local_v1_path_is_gone(self):
        record = self.seed("a prompt")
        response = self.post("/local/v1/chat/completions",
                             {"model": "x", "messages": self.child_messages(record)})
        self.assertEqual(response.status_code, 404)
        self.assertEqual(self.local_bodies, [])

    # -- one endpoint: stamping the parent's subagent calls -----------------

    def test_a_cost_parent_reply_calling_subagent_without_a_marker_gets_one(self):
        self.configure(policy="cost")
        args = json.dumps({"description": "Read the file", "prompt": "read it"})
        _, response = self.sent_to_cloud(self.later(dsh_first_turn()), ("subagent", args))
        prompt = json.loads(pls.parse_calls(response.content)[0]["arguments"])["prompt"]
        self.assertRegex(prompt, r"^pls-task: (?:[0-9a-f]{6}-)?t\d+\b")
        self.assertIn("read it", prompt)

    def test_a_cost_parent_json_reply_calling_subagent_without_a_marker_gets_one(self):
        self.configure(policy="cost")
        call = tool_call("subagent", json.dumps({"description": "d", "prompt": "read it"}))
        pls.http.send = _fake_send_json([], completion(None, [call]))
        body = {**self.later(dsh_first_turn()), "stream": False}
        message = self.cloud_post(body).json()["choices"][0]["message"]
        prompt = json.loads(message["tool_calls"][0]["function"]["arguments"])["prompt"]
        self.assertRegex(prompt, r"^pls-task: (?:[0-9a-f]{6}-)?t\d+\b")

    def test_an_ad_hoc_cost_task_resolves_on_the_child_side_as_summary(self):
        self.configure(policy="cost")
        args = json.dumps({"description": "d", "prompt": "read it"})
        _, response = self.sent_to_cloud(self.later(dsh_first_turn()), ("subagent", args))
        prompt = json.loads(pls.parse_calls(response.content)[0]["arguments"])["prompt"]
        task_id = re.match(r"pls-task: ((?:[0-9a-f]{6}-)?t\d+)", prompt).group(1)
        record = next(r for r in pls.TASKS.values() if r.get("id") == task_id)
        self.assertEqual(record["summary"], "summary")
        kind, _ = pls.resolve_child([{"role": "user", "content": prompt}], "cost")
        self.assertEqual(kind, "summary")

    def test_an_ad_hoc_privacy_task_is_local_only_and_not_private(self):
        record = pls.ad_hoc_record("privacy")
        self.assertEqual((record["summary"], record["private"]), ("local_only", False))

    def test_an_ad_hoc_cost_task_is_summary(self):
        self.assertEqual(pls.ad_hoc_record("cost")["summary"], "summary")

    def test_an_ad_hoc_task_is_found_by_resolve_child(self):
        record = pls.ad_hoc_record("privacy")
        messages = [{"role": "user", "content": f"pls-task: {record['id']}\ngo"}]
        kind, resolved = pls.resolve_child(messages, "privacy")
        self.assertEqual((kind, resolved[0]["content"]), ("local_only", "go"))

    # -- one endpoint: a subagent cannot launch a subagent ------------------

    def test_a_child_is_denied_send_message_subagent_and_subagent_fork(self):
        self.assertEqual(pls.PROFILES["deepseek"].child_denied,
                         {"send_message", "subagent", "subagent_fork"})

    def test_a_child_request_loses_its_spawning_tools_under_both_policies(self):
        for policy in ("privacy", "cost"):
            with self.subTest(policy=policy):
                self.configure(policy=policy)
                self.local_bodies.clear()
                self.post_dsh_child(self.seed("a prompt", policy=policy))
                self.assertEqual(tool_names(self.local_bodies[-1]), ["bash", "read"])

    def test_the_no_subagents_line_is_in_the_child_system_prompt_once(self):
        self.post_dsh_child(self.seed("a prompt"))
        system = self.local_bodies[-1]["messages"][0]["content"]
        self.assertEqual(system.count(NO_SUBAGENTS_LINE), 1)

    def test_a_local_only_child_gets_the_no_subagents_line_too(self):
        self.post_dsh_child(self.seed(PRIVATE_TEXT, private=True, summary="local_only"))
        system = self.local_bodies[-1]["messages"][0]["content"]
        self.assertEqual(system.count(NO_SUBAGENTS_LINE), 1)

    def test_the_no_subagents_line_is_not_added_twice(self):
        record = self.seed("a prompt")
        body = self.dsh_child(record)
        body["messages"][0]["content"] += " " + NO_SUBAGENTS_LINE
        self.post("/v1/chat/completions", body)
        system = self.local_bodies[-1]["messages"][0]["content"]
        self.assertEqual(system.count(NO_SUBAGENTS_LINE), 1)

    def spawning_local_reply(self):
        call = tool_call("subagent", json.dumps({"prompt": "go deeper"}))
        pls.http.post = _fake_return(completion("partial text", [call]))

    def test_a_local_reply_with_a_subagent_call_is_cut_back_to_its_text(self):
        self.spawning_local_reply()
        response = self.post_child(self.seed("a prompt"))
        message = response.json()["choices"][0]["message"]
        self.assertFalse(message.get("tool_calls"))
        self.assertEqual(message["content"], "partial text")

    def test_a_blocked_child_spawn_ends_the_turn(self):
        self.spawning_local_reply()
        choice = self.post_child(self.seed("a prompt")).json()["choices"][0]
        self.assertEqual(choice["finish_reason"], "stop")

    def test_a_blocked_child_spawn_is_logged(self):
        events = self.capture_log()
        self.spawning_local_reply()
        self.post_child(self.seed("a prompt"))
        self.assertIn("child_spawn_blocked", [e.get("event") for e in events])

    # -- one endpoint: cloud mode and a local model that cannot run ---------

    def test_cloud_mode_sends_a_subagent_to_the_cloud_without_its_marker(self):
        self.configure(mode="cloud")
        sent = []
        pls.http.send = _fake_send_json(sent, completion("cloud answer"))
        self.post_child(self.seed("a prompt"), "list the files")
        self.assertEqual(self.local_bodies, [])
        self.assertNotIn("pls-task:", json.dumps(sent))
        users = [m for m in sent[0]["messages"] if m["role"] == "user"]
        self.assertEqual(users[0]["content"], "list the files")

    def test_cloud_mode_does_not_restore_a_private_childs_text(self):
        self.configure(mode="cloud")
        sent = []
        pls.http.send = _fake_send_json(sent, completion("cloud answer"))
        self.post_child(self.seed(PRIVATE_TEXT, private=True, summary="local_only"))
        self.assertNotIn("billing", json.dumps(sent))

    def test_privacy_withholds_a_subagent_when_the_local_model_cannot_start(self):
        self.model_server_down()
        sent = []
        pls.http.send = _fake_send_sse(sent, None)
        response = self.post_dsh_child(self.seed("a prompt"))
        self.assertEqual(self.answer(response), pls.WITHHELD)
        self.assertEqual(sent, [])

    def test_cost_sends_a_subagent_to_the_cloud_without_its_marker_when_the_local_model_cannot_start(self):
        self.configure(policy="cost")
        self.model_server_down()
        sent = []
        pls.http.send = _fake_send_json(sent, completion("cloud answer"))
        response = self.post_child(self.seed("a prompt", policy="cost"), "list the files")
        self.assertEqual(self.answer(response), "cloud answer")
        self.assertNotIn("pls-task:", json.dumps(sent))

    def test_a_local_model_that_cannot_start_is_logged(self):
        events = self.capture_log()
        self.model_server_down()
        self.post_child(self.seed("a prompt"))
        self.assertIn("local_unavailable", [e.get("event") for e in events])

    def test_a_local_model_that_cannot_start_prints_one_privacy_line(self):
        self.model_server_down()
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            self.post_child(self.seed("a prompt"))
            self.post_child(self.seed("another prompt"))
        lines = [line for line in err.getvalue().splitlines() if line.startswith("privacy:")]
        self.assertEqual(len(lines), 1)

    # -- no subscription gate -----------------------------------------------

    def test_the_subscription_state_is_gone(self):
        for name in ("premium", "premium_cache", "ENTITLEMENT_SOCKET"):
            with self.subTest(name=name):
                self.assertFalse(hasattr(pls, name))
        self.assertNotIn("premium_blocked", pls.STATS)

    def test_debug_state_carries_no_subscription_field(self):
        self.assertNotIn("sub", self.client.get("/debug/state").json())

    def through_the_socket(self):
        """Main chat and a subagent with the real decide and token clients."""
        pls.PLS_SOCKET, seen = self.socket()
        pls.decide = REAL_DECIDE
        pls.cloud_credentials = REAL_CLOUD_CREDENTIALS
        sent = []
        pls.http.send = _fake_send_sse(sent, "subagent")
        self.cloud_post(dsh_first_turn())
        child = self.post_dsh_child(self.seed("a prompt"))
        return seen, sent, child

    def test_the_router_never_asks_the_socket_for_an_entitlement(self):
        seen, _, _ = self.through_the_socket()
        self.assertFalse([line for line in seen if "entitlement" in line])

    def test_decide_and_the_token_fetch_use_pls_socket(self):
        seen, _, _ = self.through_the_socket()
        self.assertTrue(any(line.startswith("POST /decide ") for line in seen))
        self.assertTrue(any(line.startswith("GET /token") for line in seen))

    def test_the_router_routes_hybrid_with_no_subscription_state(self):
        _, sent, child = self.through_the_socket()
        record = self.record(DSH_PROMPT)
        self.assertEqual(sent[0]["messages"][1]["content"],
                         DSH_PROMPT + "\n\n" + pls.instruction(record))
        self.assertEqual(self.answer(child), "local answer")

    def test_a_missing_config_means_hybrid_and_privacy(self):
        self.config.write_text("not json")
        config = pls.read_config()
        self.assertEqual((config["mode"], config["policy"]), ("hybrid", "privacy"))

    def test_the_removed_local_mode_falls_back_to_hybrid(self):
        self.configure(mode="local")
        self.assertEqual(pls.read_config()["mode"], "hybrid")

    def test_an_unknown_mode_falls_back_to_hybrid(self):
        self.configure(mode="sideways")
        self.assertEqual(pls.read_config()["mode"], "hybrid")

    def test_an_unknown_policy_falls_back_to_privacy(self):
        self.configure(policy="speed")
        self.assertEqual(pls.read_config()["policy"], "privacy")

    def test_the_cost_policy_is_read_from_the_config(self):
        self.configure(policy="cost")
        self.assertEqual(pls.read_config()["policy"], "cost")

    def test_read_config_returns_exactly_the_known_keys(self):
        self.assertEqual(set(pls.read_config()),
                         {"mode", "policy", "max_turns", "model_path", "cloud_model",
                          "harness", "cloud_limits"})

    def test_removed_features_are_gone(self):
        for name in ("WORK_RE", "last_is_work", "shape_hybrid", "FORCE_DELEGATION",
                     "PARENT_DENIED", "DELEGATION", "pii_instruction", "take_disclosure",
                     "DISCLOSURE_RE"):
            with self.subTest(name=name):
                self.assertFalse(hasattr(pls, name))

    def test_the_marker_is_pls_task(self):
        self.assertEqual(pls.TASK_MARKER, "pls-task:")

    def test_there_is_one_system_prompt_per_policy(self):
        self.assertNotEqual(pls.policy_prompt("privacy"), pls.policy_prompt("cost"))

    # -- finding the real prompt (real dsh data) ---------------------------

    def test_the_dsh_prompt_is_the_last_turn_despite_the_context_after_it(self):
        body = dsh_first_turn()
        self.assertEqual(pls.message_text(pls.last_turn(body)), DSH_PROMPT)

    def test_dsh_context_messages_are_injected_and_the_prompt_is_not(self):
        flags = [pls.is_injected(m) for m in dsh_first_turn()["messages"]]
        self.assertEqual(flags, [False, False, True, True, True])

    def test_prompt_key_is_a_sixteen_character_sha256_prefix(self):
        self.assertEqual(pls.prompt_key("hello"), hashlib.sha256(b"hello").hexdigest()[:16])

    def test_prompt_key_differs_for_different_prompts(self):
        self.assertNotEqual(pls.prompt_key("hello"), pls.prompt_key("hello!"))

    # -- deciding ----------------------------------------------------------

    def test_a_new_prompt_is_decided_once(self):
        self.cloud_post(dsh_first_turn())
        self.assertEqual(self.decided, [("privacy", DSH_PROMPT)])

    def test_the_decision_uses_the_configured_policy(self):
        self.configure(policy="cost")
        self.cloud_post(dsh_first_turn())
        self.assertEqual(self.decided, [("cost", DSH_PROMPT)])

    def test_a_side_call_without_tools_is_not_decided_under_cost(self):
        self.configure(policy="cost")
        # dsh's session-title request: two messages, no tools.
        self.cloud_post({"model": "x", "messages": [
            {"role": "system", "content": "Create a concise title for an AI coding-assistant session."},
            {"role": "user", "content": "Generate the session title from this JSON array of human messages"},
        ]})
        self.assertEqual(self.decided, [])

    def test_a_private_tool_less_prompt_is_span_masked(self):
        self.say("yes")
        marker = "session-title-from-the-first-prompt"
        sent = []
        pls.http.send = _fake_send_sse(sent, None)
        self.cloud_post({"model": "x", "messages": [
            {"role": "system", "content": "Create a concise title"},
            {"role": "user", "content": marker},
        ]})
        blob = json.dumps(sent)
        record = self.record(marker)
        self.assertEqual(sent[0]["messages"][1]["content"],
                         marker + "\n\n" + pls.instruction(record))
        self.assertNotIn(pls.PRIVATE_SIDE_MASK, blob)

    def test_a_tool_less_prompt_is_masked_when_the_decision_fails(self):
        self.verdict = None
        marker = "the user's first prompt"
        sent = []
        pls.http.send = _fake_send_sse(sent, None)
        self.cloud_post({"messages": [{"role": "user", "content": marker}]})
        record = self.record(marker)
        self.assertEqual(sent[0]["messages"][0]["content"],
                         marker + "\n\n" + pls.instruction(record))
        self.assertNotIn(pls.PRIVATE_SIDE_MASK, json.dumps(sent))

    def test_the_same_prompt_on_a_later_request_is_not_decided_again(self):
        self.cloud_post(dsh_first_turn())
        self.cloud_post(dsh_first_turn())
        self.assertEqual(len(self.decided), 1)

    def test_a_tool_result_turn_decides_nothing_new(self):
        body = dsh_first_turn()
        self.cloud_post(body)
        self.cloud_post(self.later(body))
        self.assertEqual(len(self.decided), 1)

    # dsh hands subagent output to the parent as user messages, in these exact
    # shapes (captured from a dsh 0.1.7 session, text replaced).
    AGENT_ID = "0691463a-dc29-4cc6-b0a4-0c9375b4a6a0"

    def agent_message(self):
        return {"role": "user", "content": [
            {"type": "text", "text": f"Agent {self.AGENT_ID} sent a message: "},
            {"type": "text", "text": "Task complete. The CSV has 25 contacts."}]}

    def agent_finished(self):
        return {"role": "user", "content": [
            {"type": "text", "text": f"Background subagent {self.AGENT_ID} finished and will do "
                                     "no further work unless you send it more."},
            {"type": "text", "text": "Its closing message:"},
            {"type": "text", "text": pls.WITHHELD}]}

    def test_a_subagent_message_is_not_decided_as_a_prompt(self):
        body = self.later(dsh_first_turn())
        body["messages"] += [self.agent_message(), self.agent_finished()]
        self.cloud_post(body)
        self.assertEqual([text for _, text in self.decided], [DSH_PROMPT])

    def test_a_reply_after_a_subagent_finished_need_not_launch_another(self):
        body = self.later(dsh_first_turn())
        body["messages"].append(self.agent_finished())
        sent, _ = self.sent_to_cloud(body, None)
        self.assertEqual(len(sent), 1)

    def test_a_subagent_message_is_masked_on_the_way_to_the_cloud_under_privacy(self):
        body = self.later(dsh_first_turn())
        body["messages"].append(self.agent_message())
        sent, _ = self.sent_to_cloud(body, None)
        self.assertEqual(sent[0]["messages"][-1]["content"], pls.AGENT_MESSAGE_MASK)

    # -- streaming through the guard ----------------------------------------

    def stream_reply(self, text, tool=None):
        """One streamed cloud reply: a text delta, then optionally a tool call."""
        events = [{"choices": [{"index": 0, "delta": {"role": "assistant", "content": text}}]}]
        if tool:
            events.append({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"name": tool, "arguments": "{}"}}]}}]})
        events.append({"choices": [{"index": 0, "delta": {},
                                    "finish_reason": "tool_calls" if tool else "stop"}]})
        body = "".join(f"data: {json.dumps(e)}\n\n" for e in events) + "data: [DONE]\n\n"
        sent = []

        async def send(request, stream=False):
            sent.append(json.loads(request.content))
            return FakeStream(body.encode())

        pls.http.send = send
        return sent

    def test_guarded_text_reaches_the_harness(self):
        sent = self.stream_reply("Here is the plan.")
        response = self.cloud_post({**self.later(dsh_first_turn()), "stream": True})
        self.assertIn("Here is the plan.", response.text)
        self.assertEqual(len(sent), 1)

    def test_a_blocked_call_after_streamed_text_is_dropped_not_asked_again(self):
        sent = self.stream_reply("Let me look.", tool="bash")
        response = self.cloud_post({**self.later(dsh_first_turn()), "stream": True})
        self.assertIn("Let me look.", response.text)
        self.assertNotIn('"bash"', response.text)
        self.assertEqual(len(sent), 1)

    def test_a_blocked_call_after_text_on_a_new_prompt_becomes_the_subagent_call(self):
        self.say("yes")
        self.stream_reply("On it.", tool="bash")
        response = self.cloud_post({**dsh_first_turn(PRIVATE_TEXT), "stream": True})
        record = self.record(PRIVATE_TEXT)
        self.assertIn('"subagent"', response.text)
        self.assertIn(f"pls-task: {record['id']}", response.text)

    def test_the_cloud_call_logs_its_timing(self):
        events = []
        original = pls.log
        pls.log = events.append
        try:
            self.stream_reply("done")
            self.cloud_post({**self.later(dsh_first_turn()), "stream": True})
        finally:
            pls.log = original
        done = [e for e in events if e.get("event") == "cloud_done"]
        self.assertTrue(done and "total_ms" in done[0] and "first_byte_ms" in done[0])
        self.assertIn("headers_ms", next(e for e in events if e.get("event") == "cloud"))

    def test_a_second_user_prompt_is_decided_as_well(self):
        body = self.later(dsh_first_turn())
        body["messages"].append({"role": "user", "content": "now run the tests"})
        self.cloud_post(body)
        self.assertEqual([text for _, text in self.decided], [DSH_PROMPT, "now run the tests"])

    def test_harness_context_is_never_decided(self):
        self.cloud_post(dsh_first_turn())
        self.assertEqual(len(self.decided), 1)
        self.assertNotIn("system-reminder", self.decided[0][1])

    def test_ensure_tasks_records_every_real_user_message(self):
        body = {"messages": [{"role": "user", "content": "one"},
                             {"role": "assistant", "content": "ok"},
                             {"role": "user", "content": "two"}]}
        asyncio.run(pls.ensure_tasks(body, "privacy"))
        ids = [pls.TASKS[key_of(t, "one")]["id"] for t in ("one", "two")]
        self.assertEqual({pls.TASK_TEXT[i] for i in ids}, {"one", "two"})

    def test_ensure_tasks_does_not_decide_a_decided_prompt_again(self):
        body = {"messages": [{"role": "user", "content": "one"}]}
        asyncio.run(pls.ensure_tasks(body, "privacy"))
        asyncio.run(pls.ensure_tasks(body, "privacy"))
        self.assertEqual(len(self.decided), 1)

    def test_task_ids_count_up_from_t1_within_a_session(self):
        body = {"messages": [{"role": "user", "content": "one"},
                             {"role": "assistant", "content": "ok"},
                             {"role": "user", "content": "two"}]}
        asyncio.run(pls.ensure_tasks(body, "privacy"))
        session = pls.prompt_key("one")[:6]
        self.assertEqual([pls.TASKS[key_of(t, "one")]["id"] for t in ("one", "two")],
                         [f"{session}-t1", f"{session}-t2"])

    def test_a_new_session_numbers_its_tasks_from_t1_again(self):
        asyncio.run(pls.ensure_tasks({"messages": [{"role": "user", "content": "one"}]}, "privacy"))
        asyncio.run(pls.ensure_tasks({"messages": [{"role": "user", "content": "other"}]}, "privacy"))
        self.assertTrue(self.record("other")["id"].endswith("-t1"))

    def test_prompts_with_no_reply_between_them_are_one_task(self):
        body = {"messages": [{"role": "user", "content": "one"},
                             {"role": "user", "content": "two"}]}
        asyncio.run(pls.ensure_tasks(body, "privacy"))
        self.assertEqual(len(self.decided), 1)
        self.assertIn(key_of("one\n\ntwo", "one"), pls.TASKS)

    def test_the_parent_sees_one_task_line_however_many_turns_it_has(self):
        body = {"messages": [{"role": "user", "content": "one"},
                             {"role": "assistant", "content": "ok"},
                             {"role": "user", "content": "two"},
                             {"role": "assistant", "content": "ok"},
                             {"role": "user", "content": "three"}]}
        asyncio.run(pls.ensure_tasks(body, "privacy"))
        shaped = pls.apply_policy(body, "privacy")
        self.assertEqual(json.dumps(shaped["messages"]).count("pls-task:"), 1)
        self.assertIn("pls-task:", json.dumps(shaped["messages"][-1]))

    def test_a_child_sent_only_the_marker_gets_its_task_text(self):
        record = self.seed("list the files", private=False)
        _, messages = pls.resolve_child(
            [{"role": "user", "content": f"pls-task: {record['id']}"}], "privacy")
        self.assertEqual(messages[0]["content"], "list the files")

    def test_a_yes_under_privacy_makes_a_private_local_only_record(self):
        self.say("yes")
        self.cloud_post(dsh_first_turn())
        record = self.record(DSH_PROMPT)
        self.assertEqual((record["policy"], record["private"], record["summary"], record["decided"]),
                         ("privacy", True, "local_only", True))

    def test_a_no_under_privacy_makes_a_summary_record(self):
        self.cloud_post(dsh_first_turn())
        record = self.record(DSH_PROMPT)
        self.assertEqual((record["private"], record["summary"], record["decided"]),
                         (False, "summary", True))

    def test_a_yes_under_cost_makes_a_big_summary_record(self):
        self.configure(policy="cost")
        self.say("yes")
        self.cloud_post(dsh_first_turn())
        record = self.record(DSH_PROMPT)
        self.assertEqual((record["policy"], record["big"], record["summary"]),
                         ("cost", True, "summary"))

    def test_a_no_under_cost_makes_a_small_record_with_no_subagent(self):
        self.configure(policy="cost")
        self.cloud_post(dsh_first_turn())
        record = self.record(DSH_PROMPT)
        self.assertEqual((record["big"], record["summary"]), (False, None))

    # -- what the cloud sees -----------------------------------------------

    def sent_to_cloud(self, body, *replies):
        sent = []
        pls.http.send = _fake_send_sse(sent, *(replies or (None,)))
        response = self.cloud_post(body)
        return sent, response

    def test_a_private_prompt_reaches_the_cloud_with_the_delegate_instruction(self):
        self.say("yes")
        sent, _ = self.sent_to_cloud(dsh_first_turn(PRIVATE_TEXT), "subagent")
        record = self.record(PRIVATE_TEXT)
        self.assertEqual(sent[0]["messages"][1]["content"],
                         PRIVATE_TEXT + "\n\n" + pls.instruction(record))

    def test_a_private_prompt_is_not_replaced_by_the_placeholder_when_masking_succeeds(self):
        self.say("yes")
        sent, _ = self.sent_to_cloud(dsh_first_turn(PRIVATE_TEXT), "subagent")
        record = self.record(PRIVATE_TEXT)
        # The hermetic classifier finds no spans, so the prompt stays and the
        # whole-prompt placeholder does not replace it.
        self.assertEqual(sent[0]["messages"][1]["content"],
                         PRIVATE_TEXT + "\n\n" + pls.instruction(record))
        self.assertNotIn("stays on this device", json.dumps(sent))

    def test_a_private_prompt_is_still_masked_on_a_later_request(self):
        self.say("yes")
        body = dsh_first_turn(PRIVATE_TEXT)
        self.sent_to_cloud(body, "subagent")
        self.say("no")  # a re-decision would unmask it
        sent, _ = self.sent_to_cloud(self.later(body), None)
        # Answered now, so the masked prompt stays and the task line does not.
        self.assertEqual(sent[0]["messages"][1]["content"], PRIVATE_TEXT)
        self.assertNotIn("pls-task:", sent[0]["messages"][1]["content"])

    def test_a_private_prompt_is_masked_after_the_cloud_asked_again(self):
        self.say("yes")
        sent, _ = self.sent_to_cloud(dsh_first_turn(PRIVATE_TEXT), "bash", "subagent")
        record = self.record(PRIVATE_TEXT)
        cloud = PRIVATE_TEXT + "\n\n" + pls.instruction(record)
        self.assertEqual(len(sent), 2)
        self.assertEqual(sent[0]["messages"][1]["content"], cloud)
        self.assertEqual(sent[1]["messages"][1]["content"], cloud)

    def test_the_placeholder_names_the_task_and_not_the_prompt(self):
        record = {"id": "t7", "private": True}
        text = pls.placeholder(record)
        self.assertIn("pls-task: t7", text)
        self.assertIn("subagent", text)

    def test_the_prompt_text_is_kept_in_memory_only(self):
        self.say("yes")
        self.sent_to_cloud(dsh_first_turn(PRIVATE_TEXT), "subagent")
        self.assertEqual(pls.TASK_TEXT[self.record(PRIVATE_TEXT)["id"]], PRIVATE_TEXT)

    def test_a_non_private_prompt_keeps_its_text_and_gets_the_delegate_instruction(self):
        sent, _ = self.sent_to_cloud(dsh_first_turn(), "subagent")
        record = self.record(DSH_PROMPT)
        self.assertEqual(sent[0]["messages"][1]["content"],
                         DSH_PROMPT + "\n\n" + pls.instruction(record))

    def test_the_delegate_instruction_names_the_task_marker(self):
        text = pls.instruction({"id": "t7", "policy": "cost", "big": True, "summary": "summary"})
        self.assertIn("pls-task: t7", text)
        self.assertIn("subagent", text)

    def test_the_small_instruction_says_to_do_it_yourself(self):
        text = pls.instruction({"id": "t7", "policy": "cost", "big": False, "summary": None})
        self.assertIn("yourself", text)
        self.assertNotIn("pls-task", text)

    def test_cost_big_gets_the_delegate_instruction(self):
        self.configure(policy="cost")
        self.say("yes")
        sent, _ = self.sent_to_cloud(dsh_first_turn(), "subagent")
        record = self.record(DSH_PROMPT)
        self.assertEqual(sent[0]["messages"][1]["content"],
                         DSH_PROMPT + "\n\n" + pls.instruction(record))
        self.assertIn(f"pls-task: {record['id']}", sent[0]["messages"][1]["content"])

    def test_cost_small_gets_the_do_it_yourself_instruction(self):
        self.configure(policy="cost")
        sent, _ = self.sent_to_cloud(dsh_first_turn(), "bash")
        record = self.record(DSH_PROMPT)
        self.assertEqual(sent[0]["messages"][1]["content"],
                         DSH_PROMPT + "\n\n" + pls.instruction(record))
        self.assertIn("yourself", sent[0]["messages"][1]["content"])

    def test_the_policy_rewrite_adds_no_message(self):
        self.say("yes")
        sent, _ = self.sent_to_cloud(dsh_first_turn(), "subagent")
        self.assertEqual(len(sent[0]["messages"]), len(dsh_first_turn()["messages"]))

    def test_the_privacy_prompt_is_added_to_the_system_message(self):
        sent, _ = self.sent_to_cloud(dsh_first_turn(), "subagent")
        self.assertIn(pls.policy_prompt("privacy"), sent[0]["messages"][0]["content"])

    def test_the_cost_prompt_is_added_to_the_system_message(self):
        self.configure(policy="cost")
        sent, _ = self.sent_to_cloud(dsh_first_turn(), "bash")
        self.assertIn(pls.policy_prompt("cost"), sent[0]["messages"][0]["content"])

    def test_the_policy_prompt_is_added_once(self):
        self.seed(DSH_PROMPT)
        once = self.applied(dsh_first_turn())
        twice = self.applied(once)
        system = twice["messages"][0]["content"]
        self.assertEqual(system.count(pls.policy_prompt("privacy")), 1)

    def test_apply_policy_masks_a_private_prompt_from_its_record(self):
        record = self.seed(PRIVATE_TEXT, private=True, summary="local_only")
        shaped = self.applied(dsh_first_turn(PRIVATE_TEXT))
        self.assertEqual(shaped["messages"][1]["content"], pls.placeholder(record))

    def test_apply_policy_leaves_harness_context_alone(self):
        self.seed(PRIVATE_TEXT, private=True, summary="local_only")
        body = dsh_first_turn(PRIVATE_TEXT)
        context = body["messages"][2:]
        self.assertEqual(self.applied(body)["messages"][2:], context)

    def test_apply_policy_leaves_a_prompt_without_a_record_alone(self):
        shaped = self.applied(dsh_first_turn())
        self.assertEqual(shaped["messages"][1]["content"], DSH_PROMPT)

    def test_privacy_offers_the_parent_only_the_subagent_tool(self):
        sent, _ = self.sent_to_cloud(dsh_first_turn(), "subagent")
        self.assertEqual(tool_names(sent[0]), ["subagent"])

    def test_cost_keeps_every_tool(self):
        self.configure(policy="cost")
        sent, _ = self.sent_to_cloud(dsh_first_turn(), "bash")
        self.assertEqual(tool_names(sent[0]), ["bash", "read", "subagent"])

    # -- the guard: privacy ------------------------------------------------

    def test_privacy_rejects_a_bash_call_and_asks_again(self):
        sent, response = self.sent_to_cloud(dsh_first_turn(), "bash", "subagent")
        record = self.record(DSH_PROMPT)
        self.assertEqual(len(sent), 2)
        nudge = sent[1]["messages"][-1]["content"]
        self.assertTrue(nudge.startswith("[router] Wrong: you answered with bash"))
        self.assertIn(f"pls-task: {record['id']}", nudge)
        self.assertEqual(pls.parse_calls(response.content)[0]["name"], "subagent")

    def test_privacy_rejects_a_subagent_call_beside_another_tool(self):
        sent, response = self.sent_to_cloud(dsh_first_turn(), "bash+subagent", "subagent")
        self.assertEqual(len(sent), 2)
        self.assertEqual(pls.parse_calls(response.content)[0]["name"], "subagent")

    def test_privacy_accepts_plain_text_when_no_prompt_is_waiting(self):
        sent, response = self.sent_to_cloud(self.later(dsh_first_turn()), None)
        self.assertEqual(len(sent), 1)
        self.assertEqual(response.status_code, 200)

    def test_privacy_rejects_plain_text_right_after_a_new_prompt(self):
        sent, response = self.sent_to_cloud(dsh_first_turn(), None, "subagent")
        self.assertEqual(len(sent), 2)
        self.assertIn("[router] Wrong: you answered with", sent[1]["messages"][-1]["content"])
        self.assertEqual(pls.parse_calls(response.content)[0]["name"], "subagent")

    def test_privacy_rejects_a_bash_call_on_a_later_turn_too(self):
        sent, _ = self.sent_to_cloud(self.later(dsh_first_turn()), "bash", "subagent")
        self.assertEqual(len(sent), 2)

    def test_after_the_retries_the_router_emits_the_subagent_call(self):
        sent, response = self.sent_to_cloud(dsh_first_turn(), None)
        record = self.record(DSH_PROMPT)
        self.assertEqual(len(sent), pls.GUARD_RETRIES + 1)
        call = pls.parse_calls(response.content)[0]
        self.assertEqual(call["name"], "subagent")
        prompt = json.loads(call["arguments"])["prompt"]
        self.assertTrue(prompt.startswith(f"pls-task: {record['id']}"))

    def test_the_emitted_call_for_a_private_task_carries_no_prompt_text(self):
        self.say("yes")
        _, response = self.sent_to_cloud(dsh_first_turn(PRIVATE_TEXT), None)
        arguments = pls.parse_calls(response.content)[0]["arguments"]
        self.assertNotIn("billing", arguments)
        self.assertIn(f"pls-task: {self.record(PRIVATE_TEXT)['id']}", arguments)

    def test_the_emitted_call_for_a_non_private_task_carries_the_task_text(self):
        _, response = self.sent_to_cloud(dsh_first_turn(), None)
        prompt = json.loads(pls.parse_calls(response.content)[0]["arguments"])["prompt"]
        self.assertIn(DSH_PROMPT, prompt)

    def test_an_accepted_subagent_call_without_the_marker_gets_it(self):
        args = json.dumps({"description": "Read the file", "prompt": "read it"})
        _, response = self.sent_to_cloud(dsh_first_turn(), ("subagent", args))
        record = self.record(DSH_PROMPT)
        prompt = json.loads(pls.parse_calls(response.content)[0]["arguments"])["prompt"]
        self.assertTrue(prompt.startswith(f"pls-task: {record['id']}"))
        self.assertIn("read it", prompt)

    def test_an_empty_subagent_call_still_gets_the_marker(self):
        _, response = self.sent_to_cloud(dsh_first_turn(), "subagent")
        record = self.record(DSH_PROMPT)
        prompt = json.loads(pls.parse_calls(response.content)[0]["arguments"])["prompt"]
        self.assertTrue(prompt.startswith(f"pls-task: {record['id']}"))

    def test_a_subagent_call_that_has_the_marker_is_not_given_a_second_one(self):
        self.say("yes")
        # The id is only known after the first decision, so decide it up front.
        asyncio.run(pls.ensure_tasks(dsh_first_turn(), "privacy"))
        marker = f"pls-task: {self.record(DSH_PROMPT)['id']}"
        args = json.dumps({"description": "d", "prompt": marker})
        _, response = self.sent_to_cloud(dsh_first_turn(), ("subagent", args))
        prompt = json.loads(pls.parse_calls(response.content)[0]["arguments"])["prompt"]
        self.assertEqual(prompt.count("pls-task:"), 1)

    def test_a_made_up_task_id_is_replaced_with_one_the_router_issued(self):
        args = json.dumps({"description": "d", "prompt": "pls-task: 71414e-t99\nread the file"})
        _, response = self.sent_to_cloud(dsh_first_turn(), ("subagent", args))
        record = self.record(DSH_PROMPT)
        prompt = json.loads(pls.parse_calls(response.content)[0]["arguments"])["prompt"]
        self.assertIn(f"pls-task: {record['id']}", prompt)
        self.assertNotIn("71414e-t99", prompt)
        self.assertIn("read the file", prompt)

    def test_a_workspace_path_is_not_span_masked(self):
        async def path_span(text):
            piece = "/Users/home/Dev/Test-project/personal_info.html"
            start = text.index(piece)
            return [{"start": start, "end": start + len(piece), "category": "private_address"}]

        pls.classify_pii = path_span
        masked = asyncio.run(pls.mask_cloud_text(
            "wrote `/Users/home/Dev/Test-project/personal_info.html`", "s1"))
        self.assertIn("/Users/home/Dev/Test-project/personal_info.html", masked)
        self.assertNotIn("⟦pii:", masked)

    def test_the_guard_runs_on_a_non_streaming_request(self):
        sent = []
        pls.http.send = _fake_send_json(
            sent, completion(None, [tool_call("bash")]), completion(None, [tool_call("subagent")]))
        body = dsh_first_turn()
        body["stream"] = False
        response = self.cloud_post(body)
        self.assertEqual(len(sent), 2)
        message = response.json()["choices"][0]["message"]
        self.assertEqual(message["tool_calls"][0]["function"]["name"], "subagent")

    # -- the guard: cost ---------------------------------------------------

    def test_cost_big_rejects_a_bash_call_right_after_the_prompt(self):
        self.configure(policy="cost")
        self.say("yes")
        sent, response = self.sent_to_cloud(dsh_first_turn(), "bash", "subagent")
        self.assertEqual(len(sent), 2)
        self.assertIn("[router] Wrong: you answered with bash", sent[1]["messages"][-1]["content"])
        self.assertEqual(pls.parse_calls(response.content)[0]["name"], "subagent")

    def test_cost_big_rejects_plain_text_right_after_the_prompt(self):
        self.configure(policy="cost")
        self.say("yes")
        sent, _ = self.sent_to_cloud(dsh_first_turn(), None, "subagent")
        self.assertEqual(len(sent), 2)

    def test_cost_big_accepts_a_bash_call_on_a_later_turn(self):
        self.configure(policy="cost")
        self.say("yes")
        sent, response = self.sent_to_cloud(self.later(dsh_first_turn()), "bash")
        self.assertEqual(len(sent), 1)
        self.assertEqual(pls.parse_calls(response.content)[0]["name"], "bash")

    def test_cost_small_accepts_a_bash_call_right_after_the_prompt(self):
        self.configure(policy="cost")
        sent, response = self.sent_to_cloud(dsh_first_turn(), "bash")
        self.assertEqual(len(sent), 1)
        self.assertEqual(pls.parse_calls(response.content)[0]["name"], "bash")

    def test_cost_small_accepts_plain_text_right_after_the_prompt(self):
        self.configure(policy="cost")
        sent, _ = self.sent_to_cloud(dsh_first_turn(), None)
        self.assertEqual(len(sent), 1)

    def test_cost_big_emits_the_subagent_call_after_the_retries(self):
        self.configure(policy="cost")
        self.say("yes")
        sent, response = self.sent_to_cloud(dsh_first_turn(), "bash")
        self.assertEqual(len(sent), pls.GUARD_RETRIES + 1)
        call = pls.parse_calls(response.content)[0]
        self.assertEqual(call["name"], "subagent")
        self.assertIn(f"pls-task: {self.record(DSH_PROMPT)['id']}", call["arguments"])

    # -- the decider is down -----------------------------------------------

    def test_privacy_masks_the_prompt_when_the_decider_is_down(self):
        self.verdict = None
        sent, _ = self.sent_to_cloud(dsh_first_turn(PRIVATE_TEXT), "subagent")
        record = self.record(PRIVATE_TEXT)
        self.assertEqual(sent[0]["messages"][1]["content"],
                         PRIVATE_TEXT + "\n\n" + pls.instruction(record))
        self.assertNotIn("stays on this device", json.dumps(sent))

    def test_a_failed_privacy_decision_is_local_only_and_undecided(self):
        self.verdict = None
        self.sent_to_cloud(dsh_first_turn(), "subagent")
        record = self.record(DSH_PROMPT)
        self.assertEqual((record["summary"], record["decided"]), ("local_only", False))

    def test_cost_treats_the_prompt_as_small_when_the_decider_is_down(self):
        self.configure(policy="cost")
        self.verdict = None
        sent, _ = self.sent_to_cloud(dsh_first_turn(), "bash")
        record = self.record(DSH_PROMPT)
        self.assertEqual(sent[0]["messages"][1]["content"],
                         DSH_PROMPT + "\n\n" + pls.instruction(record))
        self.assertIn("yourself", sent[0]["messages"][1]["content"])

    def test_a_failed_cost_decision_is_small_and_undecided(self):
        self.configure(policy="cost")
        self.verdict = None
        self.sent_to_cloud(dsh_first_turn(), "bash")
        record = self.record(DSH_PROMPT)
        self.assertEqual((record["summary"], record["decided"]), (None, False))

    def test_an_undecided_prompt_is_decided_again_on_the_next_request(self):
        self.verdict = None
        body = dsh_first_turn()
        self.sent_to_cloud(body, "subagent")
        self.say("no")
        sent, _ = self.sent_to_cloud(body, "subagent")
        self.assertEqual(len(self.decided), 2)
        self.assertTrue(self.record(DSH_PROMPT)["decided"])
        self.assertIn(DSH_PROMPT, sent[0]["messages"][1]["content"])

    # -- the persisted records --------------------------------------------

    def test_tasks_json_holds_the_masked_prompt_and_the_session(self):
        self.say("yes")
        self.sent_to_cloud(dsh_first_turn(PRIVATE_TEXT), "subagent")
        saved = json.loads(pls.TASKS_PATH.read_text())
        record = saved[key_of(PRIVATE_TEXT)]
        self.assertEqual(record["masked"], PRIVATE_TEXT)
        self.assertEqual(record["mask_session"], "s")
        self.assertNotIn("prompt", record)

    def test_a_cost_decision_from_an_earlier_launch_is_decided_again(self):
        # A stale "small" from an older question must not decide this launch.
        pls.TASKS_PATH.write_text(json.dumps({key_of(DSH_PROMPT): {
            "id": "t5", "policy": "cost", "private": False, "big": False,
            "summary": None, "decided": True}}))
        pls.load_tasks()
        self.configure(policy="cost")
        self.cloud_post(dsh_first_turn())
        self.assertEqual(self.decided, [("cost", DSH_PROMPT)])

    def test_tasks_json_is_readable_by_its_owner_only(self):
        self.sent_to_cloud(dsh_first_turn(), "subagent")
        self.assertEqual(stat.S_IMODE(pls.TASKS_PATH.stat().st_mode), 0o600)

    # -- PII ---------------------------------------------------------------

    def test_pii_counts_are_per_file_and_never_carry_values(self):
        found = pls.scan_pii([{"role": "tool", "content":
            "/work/a.csv: alice@x.com (555) 010-0101\n/work/b.csv: bob@y.com\n"}])
        self.assertEqual(found["files"]["/work/a.csv"], {"email": 1, "phone": 1})
        self.assertEqual(found["files"]["/work/b.csv"], {"email": 1})
        self.assertEqual(found["total"], 3)
        self.assertNotIn("@", json.dumps(found))

    def test_pii_ignores_text_without_pii(self):
        self.assertEqual(pls.scan_pii([{"role": "user", "content": "hello"}]), {"files": {}, "total": 0})

    def test_the_values_are_redacted_before_the_cloud_sees_them(self):
        prompt = "summarise /w/a.csv, maya@x.com, +44 7700 900123"
        sent, _ = self.sent_to_cloud(dsh_first_turn(prompt), "subagent")
        sent_text = json.dumps(sent[0]["messages"])
        self.assertNotIn("maya@x.com", sent_text)
        self.assertIn("[email]", sent_text)
        self.assertIn("/w/a.csv", sent_text)

    def test_a_pii_tool_result_is_redacted_before_the_cloud_sees_it(self):
        body = self.later(dsh_first_turn(), "/w/a.csv: maya@x.com")
        sent, _ = self.sent_to_cloud(body, None)
        self.assertNotIn("maya@x.com", json.dumps(sent[0]["messages"]))

    def test_the_pii_counts_go_to_the_cloud_in_a_header(self):
        captured = []

        async def send(request, stream=False):
            captured.append(request)
            return FakeStream(_sse(None))

        pls.http.send = send
        self.cloud_post(self.later(dsh_first_turn(), "/w/a.csv: maya@x.com"))
        header = captured[0].headers["x-pls-pii"]
        self.assertEqual(json.loads(header), {"/w/a.csv": {"email": 1}})
        self.assertNotIn("maya@x.com", header)

    def test_a_turn_without_pii_sends_no_pii_header(self):
        captured = []

        async def send(request, stream=False):
            captured.append(request)
            return FakeStream(_sse("subagent"))

        pls.http.send = send
        self.cloud_post(dsh_first_turn())
        self.assertNotIn("x-pls-pii", captured[0].headers)

    # -- parse_calls -------------------------------------------------------

    def test_parse_calls_joins_arguments_split_across_chunks(self):
        body = (
            b'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":'
            b'{"name":"subagent","arguments":"{\\"prompt\\":"}}]}}]}\n\n'
            b'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":'
            b'{"arguments":"\\"go\\"}"}}]}}]}\n\n'
            b"data: [DONE]\n\n"
        )
        calls = pls.parse_calls(body)
        self.assertEqual(calls[0]["name"], "subagent")
        self.assertEqual(json.loads(calls[0]["arguments"]), {"prompt": "go"})

    # -- launch token ------------------------------------------------------

    def test_stats_and_debug_require_the_launch_token(self):
        pls.TOKEN = "s3cret"
        try:
            for path in ("/stats", "/debug/state", "/debug/export"):
                with self.subTest(path=path):
                    self.assertEqual(self.client.get(path).status_code, 401)
            self.assertEqual(self.client.post("/stats/reset").status_code, 401)
            self.assertEqual(asyncio.run(self._events_status()), 401)
        finally:
            pls.TOKEN = ""

    async def _events_status(self):
        transport = httpx.ASGITransport(app=pls.app)
        async with httpx.AsyncClient(transport=transport, base_url="http://router") as client:
            response = await client.get("/debug/events")
            return response.status_code

    def test_stats_accepts_the_launch_token(self):
        pls.TOKEN = "s3cret"
        try:
            response = self.client.get("/stats", headers={"x-pls-token": "s3cret"})
            self.assertEqual(response.status_code, 200)
        finally:
            pls.TOKEN = ""

    def test_a_request_without_the_launch_token_is_refused(self):
        pls.TOKEN = "s3cret"
        try:
            self.assertEqual(self.post("/v1/chat/completions").status_code, 401)
        finally:
            pls.TOKEN = ""

    def test_a_request_with_the_launch_token_is_served(self):
        pls.TOKEN = "s3cret"
        try:
            pls.http.send = _fake_send_sse([], None)
            response = self.post("/v1/chat/completions", {
                "messages": [{"role": "user", "content": "hi"}],
            }, headers={"x-session-id": "s", "x-pls-token": "s3cret"})
            self.assertEqual(response.status_code, 200)
        finally:
            pls.TOKEN = ""

    # -- the queue ---------------------------------------------------------

    def full_queue_child_request(self, record):
        """Post a child request while the one local slot is held and the queue is full.

        Returns (finished while waiting, the response).
        """
        body = {"model": "x", "messages": self.child_messages(record)}

        async def scenario():
            await pls.local_sem.acquire()
            pls.waiting = pls.QUEUE_MAX
            transport = httpx.ASGITransport(app=pls.app)
            async with httpx.AsyncClient(transport=transport, base_url="http://router") as client:
                task = asyncio.create_task(client.post(
                    "/v1/chat/completions", json=body, headers={"x-session-id": "s"}))
                await asyncio.sleep(0.05)
                finished = task.done()
                pls.local_sem.release()
                return finished, await asyncio.wait_for(task, 2)

        return asyncio.run(scenario())

    def test_a_private_child_waits_instead_of_overflowing(self):
        sent = []
        pls.http.send = _fake_send_sse(sent, None)
        record = self.seed(PRIVATE_TEXT, private=True, summary="local_only")
        finished, response = self.full_queue_child_request(record)
        self.assertFalse(finished)
        self.assertEqual(sent, [])  # never went to the cloud
        self.assertEqual(response.status_code, 200)

    def test_privacy_never_overflows_a_non_private_child_either(self):
        sent = []
        pls.http.send = _fake_send_sse(sent, None)
        finished, response = self.full_queue_child_request(self.seed("a prompt"))
        self.assertFalse(finished)
        self.assertEqual(sent, [])
        self.assertEqual(response.status_code, 200)

    def test_cancelling_a_queued_request_drops_waiting(self):
        record = self.seed("a prompt")

        async def scenario():
            await pls.local_sem.acquire()
            pls.waiting = 0
            transport = httpx.ASGITransport(app=pls.app)
            async with httpx.AsyncClient(transport=transport, base_url="http://router") as client:
                task = asyncio.create_task(client.post(
                    "/v1/chat/completions",
                    json={"model": "x", "messages": self.child_messages(record)},
                    headers={"x-session-id": "s"},
                ))
                for _ in range(50):
                    if pls.waiting == 1:
                        break
                    await asyncio.sleep(0.01)
                self.assertEqual(pls.waiting, 1)
                task.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await task
                await asyncio.sleep(0.05)
            pls.local_sem.release()
            return pls.waiting

        self.assertEqual(asyncio.run(scenario()), 0)

    def test_a_local_only_child_does_not_overflow_under_cost(self):
        self.configure(policy="cost")
        sent = []
        pls.http.send = _fake_send_sse(sent, None)
        record = self.seed("secret file body", policy="cost", summary="local_only")
        finished, response = self.full_queue_child_request(record)
        self.assertFalse(finished)
        self.assertEqual(sent, [])
        self.assertEqual(response.status_code, 200)

    def test_cost_overflows_a_child_to_the_cloud_when_the_queue_is_full(self):
        self.configure(policy="cost")
        pls.http.send = _fake_send(completion("cloud answer"))
        record = self.seed("a prompt", policy="cost", big=True)
        finished, response = self.full_queue_child_request(record)
        self.assertTrue(finished)
        self.assertEqual(response.headers["x-pls-overflow"], "1")
        self.assertEqual(self.local_bodies, [])

    # -- the decision client ----------------------------------------------

    def test_decide_waits_as_long_as_the_server_allows(self):
        seen = {}
        real = asyncio.wait_for

        async def wrapped(awaitable, timeout):
            seen["timeout"] = timeout
            return await real(awaitable, timeout)

        asyncio.wait_for = wrapped
        try:
            pls.PLS_SOCKET, _ = self.decide_socket(
                payload={"answer": "yes", "p_yes": 0.9, "model": "m", "ms": 1})
            result = asyncio.run(REAL_DECIDE("privacy", "slow"))
        finally:
            asyncio.wait_for = real
        self.assertEqual(result["answer"], "yes")
        self.assertGreaterEqual(seen["timeout"], 120)

    def test_credentials_are_opened_at_mode_0600(self):
        modes = []
        real_open = os.open

        def wrapped(path, flags, mode=0o777, *args, **kwargs):
            if str(path).endswith(".json.tmp"):
                modes.append(mode & 0o777)
            return real_open(path, flags, mode, *args, **kwargs)

        os.open = wrapped
        try:
            pls.write_credentials({"access_token": "a", "refresh_token": "b"})
        finally:
            os.open = real_open
        self.assertEqual(modes, [0o600])
        self.assertEqual(stat.S_IMODE(self.creds.stat().st_mode), 0o600)

    def test_cloud_credentials_take_the_socket_token_and_leave_the_file(self):
        pls.PLS_SOCKET = self.token_socket()
        before = self.creds.read_text()
        creds = asyncio.run(REAL_CLOUD_CREDENTIALS())
        self.assertEqual(creds["access_token"], "access-from-wally")
        self.assertEqual(self.creds.read_text(), before)

    def token_socket(self, access="access-from-wally", console="http://cloud"):
        path = str(self.dir / "token.sock")
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
        server = socket_mod.socket(socket_mod.AF_UNIX, socket_mod.SOCK_STREAM)
        server.bind(path)
        server.listen(1)

        def serve():
            try:
                conn, _ = server.accept()
            except OSError:
                return
            conn.recv(512)
            body = json.dumps({"access_token": access, "console_url": console}).encode()
            conn.sendall(
                b"HTTP/1.1 200 OK\r\ncontent-length: " + str(len(body)).encode() + b"\r\n\r\n" + body
            )
            conn.close()
            server.close()

        threading.Thread(target=serve, daemon=True).start()
        return path

    def test_decide_returns_the_answer_and_probability_from_the_socket(self):
        pls.PLS_SOCKET, _ = self.decide_socket(
            payload={"answer": "yes", "p_yes": 0.9, "model": "clef-flash-9b", "ms": 3})
        result = asyncio.run(REAL_DECIDE("privacy", "some text"))
        self.assertEqual((result["answer"], result["p_yes"]), ("yes", 0.9))

    def test_decide_posts_the_policy_and_text_to_the_decide_path(self):
        pls.PLS_SOCKET, seen = self.decide_socket(
            payload={"answer": "no", "p_yes": 0.1, "model": "m", "ms": 1})
        asyncio.run(REAL_DECIDE("cost", "some text"))
        line, body = seen[0]
        self.assertTrue(line.startswith("POST /decide"))
        self.assertEqual(json.loads(body), {"policy": "cost", "text": "some text"})

    def test_decide_is_none_when_the_service_is_unavailable(self):
        pls.PLS_SOCKET, _ = self.decide_socket(status=503, payload={"error": "no decider"})
        self.assertIsNone(asyncio.run(REAL_DECIDE("privacy", "some text")))

    def test_decide_is_none_for_a_reply_that_is_not_json(self):
        pls.PLS_SOCKET, _ = self.decide_socket(raw=b"not json")
        self.assertIsNone(asyncio.run(REAL_DECIDE("privacy", "some text")))

    def test_decide_is_none_for_an_answer_that_is_neither_yes_nor_no(self):
        pls.PLS_SOCKET, _ = self.decide_socket(payload={"answer": "maybe", "p_yes": 0.5})
        self.assertIsNone(asyncio.run(REAL_DECIDE("privacy", "some text")))

    def test_decide_is_none_when_no_socket_is_configured(self):
        pls.PLS_SOCKET = ""
        self.assertIsNone(asyncio.run(REAL_DECIDE("privacy", "some text")))

    def test_decide_is_none_when_the_socket_is_not_there(self):
        pls.PLS_SOCKET = str(self.dir / "gone.sock")
        self.assertIsNone(asyncio.run(REAL_DECIDE("privacy", "some text")))


class FakeStream:
    def __init__(self, body: bytes, status=200):
        self._body = body
        self.status_code = status
        self.headers = {"content-type": "text/event-stream"}
        self.content = body
        self.text = body.decode()

    async def aiter_bytes(self):
        yield self._body

    async def aread(self):
        return self._body

    async def aclose(self):
        pass


def _sse(name, args="{}"):
    parts = []
    if name:
        for index, one in enumerate(name.split("+")):
            parts.append("data: " + json.dumps({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": index, "function": {"name": one, "arguments": args}}]}}]}) + "\n\n")
    parts.append("data: " + json.dumps({"choices": [{"index": 0, "delta": {},
                 "finish_reason": "tool_calls" if name else "stop"}]}) + "\n\n")
    parts.append("data: [DONE]\n\n")
    return "".join(parts).encode()


def _fake_send_sse(sent, *replies):
    """Answer successive cloud sends with these tool names; the last repeats.

    A reply is a tool name ("a+b" for two calls at once), None for plain text,
    or a (name, arguments) tuple.
    """
    queue = list(replies)

    async def send(request, stream=False):
        sent.append(json.loads(request.content))
        reply = queue.pop(0) if len(queue) > 1 else (queue[0] if queue else None)
        if isinstance(reply, tuple):
            return FakeStream(_sse(*reply))
        return FakeStream(_sse(reply))

    return send


def _fake_send_json(sent, *payloads):
    """Answer successive cloud sends with these JSON completions; the last repeats."""
    queue = list(payloads)

    async def send(request, stream=False):
        sent.append(json.loads(request.content))
        return FakeResponse(queue.pop(0) if len(queue) > 1 else queue[0])

    return send


def _fake_return(payload):
    async def post(url, json=None, headers=None):
        return FakeResponse(payload)

    return post


def _fake_send(payload):
    async def send(request, stream=False):
        return FakeResponse(payload)

    return send


class ProfileTest(unittest.TestCase):
    """Each harness profile's spawn tool, child signal and prompt cleanup."""

    # RouterTest's fixture and helpers, without re-running its tests.
    configure = RouterTest.configure
    seed = RouterTest.seed
    post = RouterTest.post
    child_messages = RouterTest.child_messages

    def setUp(self):
        RouterTest.setUp(self)
        self.addCleanup(pls.PROFILE.reset, pls.PROFILE.set(pls.PROFILES["deepseek"]))

    def use(self, harness, policy="privacy"):
        """Select `harness` for direct calls and for requests through the route."""
        pls.PROFILE.set(pls.PROFILES[harness])
        self.config.write_text(json.dumps({"mode": "hybrid", "policy": policy,
                                           "max_turns": 20, "harness": harness}))

    def stamped(self, name, arguments):
        """A parent call to `name` after the router stamped it with a fresh task."""
        record = self.seed("a prompt", summary="summary")
        calls = {0: {"name": name, "arguments": json.dumps(arguments)}}
        pls.stamp_subagent_calls(calls, record, "cost")
        return record, json.loads(calls[0]["arguments"])

    # -- the spawn call -----------------------------------------------------

    def test_each_profile_stamps_its_own_spawn_field(self):
        cases = {
            "deepseek": ("subagent", {"prompt": "go"}, lambda a: a["prompt"]),
            "opencode-v2": ("subagent", {"prompt": "go"}, lambda a: a["prompt"]),
            "opencode-v1": ("task", {"prompt": "go"}, lambda a: a["prompt"]),
            "hermes": ("delegate_task", {"tasks": [{"goal": "go"}]}, lambda a: a["tasks"][0]["goal"]),
            "openclaw": ("sessions_spawn", {"task": "go"}, lambda a: a["task"]),
        }
        for harness, (tool, arguments, field) in cases.items():
            with self.subTest(harness=harness):
                self.use(harness)
                record, patched = self.stamped(tool, arguments)
                self.assertEqual(field(patched), f"pls-task: {record['id']}\n\ngo")

    def test_every_hermes_task_in_one_call_is_stamped(self):
        self.use("hermes")
        record, patched = self.stamped("delegate_task", {"tasks": [{"goal": "a"}, {"goal": "b"}]})
        self.assertEqual([t["goal"] for t in patched["tasks"]],
                         [f"pls-task: {record['id']}\n\na", f"pls-task: {record['id']}\n\nb"])

    def test_a_hermes_list_action_is_not_a_spawn_and_is_left_alone(self):
        self.use("hermes")
        calls = {0: {"name": "delegate_task", "arguments": '{"action": "list"}'}}
        self.assertFalse(pls.stamp_subagent_calls(calls, self.seed("p"), "cost"))
        self.assertEqual(calls[0]["arguments"], '{"action": "list"}')

    def test_another_profiles_spawn_tool_is_not_stamped(self):
        self.use("hermes")
        calls = {0: {"name": "subagent", "arguments": '{"prompt": "go"}'}}
        self.assertFalse(pls.stamp_subagent_calls(calls, self.seed("p"), "cost"))

    def test_the_synthesised_call_carries_each_harness_required_fields(self):
        expected = {
            "deepseek": ("subagent", {"description": "Run the task on this device"}),
            "opencode-v2": ("subagent", {"agent": "general", "description": "Run the task on this device"}),
            "opencode-v1": ("task", {"subagent_type": "general", "description": "Run the task on this device"}),
            "hermes": ("delegate_task", {}),
            "openclaw": ("sessions_spawn", {}),
        }
        for harness, (tool, fields) in expected.items():
            with self.subTest(harness=harness):
                self.use(harness)
                record = self.seed("a prompt")
                body = json.loads(pls.synthetic_json(record).body)
                call = body["choices"][0]["message"]["tool_calls"][0]["function"]
                arguments = json.loads(call["arguments"])
                self.assertEqual(call["name"], tool)
                for name, value in fields.items():
                    self.assertEqual(arguments[name], value)
                prompts = pls.call_prompts({"name": tool, "arguments": call["arguments"]})
                self.assertEqual(prompts, [f"pls-task: {record['id']}\n\na prompt"])

    def test_an_openclaw_spawn_cannot_name_a_model(self):
        self.use("openclaw")
        _, patched = self.stamped("sessions_spawn", {"task": "go", "model": "cloud/big"})
        self.assertNotIn("model", patched)

    def test_an_already_marked_openclaw_spawn_still_loses_its_model(self):
        self.use("openclaw")
        calls = {0: {"name": "sessions_spawn",
                     "arguments": json.dumps({"task": "pls-task: t1\ngo", "model": "x"})}}
        self.assertTrue(pls.stamp_subagent_calls(calls, None, "cost"))
        self.assertNotIn("model", json.loads(calls[0]["arguments"]))

    # -- the guard and the parent's tools --------------------------------------

    def test_privacy_lets_each_parent_call_only_its_spawn_tool_for_a_pending_task(self):
        for harness in pls.PROFILES:
            with self.subTest(harness=harness):
                self.use(harness)
                record = self.seed("p")
                spawn = pls.PROFILES[harness].spawn_tool
                self.assertTrue(pls.guard_verdict({spawn}, "privacy", record))
                self.assertFalse(pls.guard_verdict({"bash"}, "privacy", record))

    def test_openclaw_privacy_keeps_the_tools_that_collect_results(self):
        self.use("openclaw")
        body = {"messages": [{"role": "system", "content": "s"}],
                "tools": tool_defs("exec", "sessions_spawn", "subagents", "sessions_history")}
        self.assertEqual(tool_names(pls.apply_policy(body, "privacy")),
                         ["sessions_spawn", "subagents", "sessions_history"])
        self.assertTrue(pls.guard_verdict({"subagents"}, "privacy", None))

    def test_the_policy_prompt_names_the_profiles_tool_and_field(self):
        self.use("hermes")
        self.assertIn("`delegate_task`", pls.policy_prompt("privacy"))
        self.assertIn("goal", pls.policy_prompt("cost"))
        self.assertIn("`delegate_task`", pls.instruction(self.seed("p", summary="summary")))

    # -- the child side ----------------------------------------------------------

    def test_a_parent_session_header_makes_a_request_a_subagent(self):
        body = {"model": "Hybrid-Wally-0.1", "tools": tool_defs("bash"),
                "messages": [{"role": "user", "content": "hi"}]}
        self.assertEqual(pls.classify(body, {"x-parent-session-id": "ses_1"}), "subagent")
        self.assertEqual(pls.classify(body, {"x-session-id": "ses_1"}), "main")
        self.assertEqual(pls.classify(body), "main")

    def test_a_parent_session_header_routes_the_request_to_the_local_model(self):
        self.use("opencode-v2")
        body = {"model": "Hybrid-Wally-0.1", "stream": False, "tools": tool_defs("bash"),
                "messages": [{"role": "user", "content": "look around"}]}
        self.post("/v1/chat/completions", body, {"x-parent-session-id": "ses_1"})
        self.assertEqual(len(self.local_bodies), 1)

    def test_the_opencode_v2_preface_is_removed_before_the_marker_is_read(self):
        self.use("opencode-v2")
        record = self.seed("the real task")
        messages = [{"role": "user", "content":
                     f"You are a subagent spawned by another session.\npls-task: {record['id']}\nmore"}]
        self.assertEqual(pls.find_marker(messages)[1].group(1), record["id"])
        _, resolved = pls.resolve_child(messages, "cost")
        self.assertEqual(resolved[0]["content"], "more")

    def test_the_openclaw_context_block_is_kept_and_the_marker_still_found(self):
        self.use("openclaw")
        record = self.seed("the real task")
        envelope = (f"{pls.OPENCLAW_CONTEXT}\n[Subagent Context] depth 1/1\n"
                    "<<<END_OPENCLAW_INTERNAL_CONTEXT>>>\n")
        messages = [{"role": "user", "content": f"{envelope}pls-task: {record['id']}\ngo"}]
        _, resolved = pls.resolve_child(messages, "cost")
        self.assertTrue(resolved[0]["content"].startswith(pls.OPENCLAW_CONTEXT))
        self.assertTrue(resolved[0]["content"].endswith("go"))
        self.assertNotIn("pls-task:", resolved[0]["content"])

    def test_each_profiles_child_loses_its_spawning_tools(self):
        for harness, profile in pls.PROFILES.items():
            with self.subTest(harness=harness):
                self.use(harness)
                self.local_bodies.clear()
                body = {"model": "Hybrid-Wally-0.1-local", "stream": False,
                        "tools": tool_defs("bash", *sorted(profile.child_denied)),
                        "messages": self.child_messages(self.seed("p"))}
                self.post("/v1/chat/completions", body)
                self.assertEqual(tool_names(self.local_bodies[-1]), ["bash"])

    def test_dsh_injected_text_is_a_prompt_to_other_profiles(self):
        message = {"role": "user", "content": "Current runtime context. cwd=/x"}
        self.assertTrue(pls.is_injected(message))
        self.use("hermes")
        self.assertFalse(pls.is_injected(message))

    def test_an_openclaw_result_announcement_is_not_the_persons_prompt(self):
        self.use("openclaw")
        message = {"role": "user", "content": f"{pls.OPENCLAW_CONTEXT}\nresult"}
        self.assertTrue(pls.is_agent_message(message))
        self.assertFalse(pls.is_prompt(message))

    # -- config and models ------------------------------------------------------

    def test_an_unknown_harness_falls_back_to_dsh_and_aliases_resolve(self):
        self.config.write_text(json.dumps({"harness": "nope"}))
        self.assertEqual(pls.read_config()["harness"], "deepseek")
        self.config.write_text(json.dumps({"harness": "openwork"}))
        self.assertEqual(pls.read_config()["harness"], "opencode-v1")

    def test_models_carry_context_and_output_limits(self):
        self.config.write_text(json.dumps({"cloud_limits": {
            "context_length": 200000, "max_completion_tokens": 32000}}))
        rows = {row["id"]: row for row in self.client.get("/v1/models").json()["data"]}
        self.assertEqual(rows["Hybrid-Wally-0.1"]["context_length"], 200000)
        self.assertEqual(rows["Hybrid-Wally-0.1"]["max_completion_tokens"], 32000)
        self.assertEqual(rows["Hybrid-Wally-0.1-local"], {
            "id": "Hybrid-Wally-0.1-local", "object": "model", "owned_by": "wally",
            **pls.LOCAL_LIMITS})

    def test_a_bad_cloud_limit_falls_back_to_the_local_one(self):
        self.config.write_text(json.dumps({"cloud_limits": {"context_length": "big"}}))
        self.assertEqual(pls.read_config()["cloud_limits"], pls.LOCAL_LIMITS)


if __name__ == "__main__":
    unittest.main()
