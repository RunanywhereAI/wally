"""Router tests that never touch the model or the network.

The model server and the cloud are both replaced at the module boundary, so
these run in a second.
"""

import json
import os
import socket as socket_mod
import tempfile
import unittest
from pathlib import Path

os.environ.setdefault("WALLY_PROFILE_DIR", tempfile.mkdtemp())

import pls  # noqa: E402
from fastapi.testclient import TestClient  # noqa: E402


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


class RouterTest(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        self.config = self.dir / "config.json"
        self.config.write_text(json.dumps({"mode": "hybrid", "max_turns": 20}))
        self.creds = self.dir / "credentials.json"
        self.creds.write_text(json.dumps({"console_url": "http://cloud", "access_token": "t"}))

        pls.CONFIG_PATH = self.config
        pls.CREDENTIALS_PATH = self.creds
        pls.turns.clear()
        pls.ENTITLEMENT_SOCKET = ""
        pls.FORCE_DELEGATION = False
        pls.waiting = 0
        pls.local_sem = __import__("asyncio").Semaphore(1)

        self.local_bodies = []
        self.client = TestClient(pls.app)

        async def fake_server():
            return 1

        async def fake_post(url, json=None, headers=None):
            self.local_bodies.append(json)
            return FakeResponse(completion("local answer"))

        async def fake_creds():
            return {"console_url": "http://cloud", "access_token": "t"}

        pls.ensure_model_server = fake_server
        pls.http.post = fake_post
        pls.cloud_credentials = fake_creds

    def mode(self, value):
        self.config.write_text(json.dumps({"mode": value, "max_turns": 20}))

    def post(self, path, body=None):
        return self.client.post(
            path,
            json=body or {"model": "x", "messages": [{"role": "user", "content": "hi"}]},
            headers={"x-session-id": "s"},
        )

    def test_local_lane_returns_the_model_answer(self):
        response = self.post("/local/v1/chat/completions")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["choices"][0]["message"]["content"], "local answer")

    def test_local_only_withholds_the_answer(self):
        response = self.post("/local-only/v1/chat/completions")
        self.assertEqual(response.json()["choices"][0]["message"]["content"], pls.WITHHELD)

    def test_tool_calls_pass_through_undisclosed(self):
        calls = [{"id": "c", "type": "function", "function": {"name": "f", "arguments": "{}"}}]

        async def with_calls(url, json=None, headers=None):
            return FakeResponse(completion(None, calls))

        pls.http.post = with_calls
        response = self.post("/local-only/v1/chat/completions")
        message = response.json()["choices"][0]["message"]
        self.assertEqual(message["tool_calls"], calls)
        self.assertIsNone(message["content"])

    def test_cap_appends_wrap_up_and_disables_tools(self):
        self.config.write_text(json.dumps({"mode": "hybrid", "max_turns": 1}))
        self.post("/local/v1/chat/completions")
        sent = self.local_bodies[-1]
        self.assertEqual(sent["tool_choice"], "none")
        self.assertEqual(sent["messages"][-1]["content"], pls.WRAP_UP)

    def test_cloud_mode_forces_the_cloud_lane(self):
        self.mode("cloud")
        pls.http.send = _fake_send(completion("cloud answer"))
        response = self.post("/local/v1/chat/completions")
        self.assertEqual(response.json()["choices"][0]["message"]["content"], "cloud answer")
        self.assertEqual(self.local_bodies, [])

    def test_local_mode_forces_the_local_lane(self):
        self.mode("local")
        response = self.post("/v1/chat/completions")
        self.assertEqual(response.json()["choices"][0]["message"]["content"], "local answer")

    def test_unknown_mode_falls_back_to_hybrid(self):
        self.config.write_text("not json")
        self.assertEqual(pls.read_config()["mode"], "hybrid")

    def test_pii_counts_are_per_file_and_never_carry_values(self):
        found = pls.scan_pii([{"role": "tool", "content":
            "/work/a.csv: alice@x.com 4111 1111 1111 1111\n/work/b.csv: bob@y.com\n"}])
        self.assertEqual(found["files"]["/work/a.csv"], {"email": 1, "phone": 1})
        self.assertEqual(found["files"]["/work/b.csv"], {"email": 1})
        self.assertEqual(found["total"], 3)
        self.assertNotIn("@", json.dumps(found))

    def test_pii_ignores_text_without_pii(self):
        self.assertEqual(pls.scan_pii([{"role": "user", "content": "hello"}]), {"files": {}, "total": 0})

    def test_no_subscription_forces_the_cloud_lane(self):
        self.mode("local")
        pls.ENTITLEMENT_SOCKET = self.socket(False)
        pls.premium_cache.update(value=False, at=0.0)
        pls.http.send = _fake_send(completion("cloud answer"))
        response = self.post("/local/v1/chat/completions")
        self.assertEqual(response.json()["choices"][0]["message"]["content"], "cloud answer")
        self.assertEqual(self.local_bodies, [])

    def test_subscription_allows_the_local_lane(self):
        pls.ENTITLEMENT_SOCKET = self.socket(True)
        pls.premium_cache.update(value=True, at=0.0)
        response = self.post("/local/v1/chat/completions")
        self.assertEqual(response.json()["choices"][0]["message"]["content"], "local answer")

    def test_shape_drops_parent_file_tools_and_forces_delegation_on_work(self):
        pls.FORCE_DELEGATION = True
        body = {
            "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "read /w/a.csv"}],
            "tools": [{"type": "function", "function": {"name": n}} for n in ("read", "grep", "bash", "subagent")],
        }
        remaining, removed, forced, _ = pls.shape_hybrid(body)
        self.assertEqual(remaining, 2)
        self.assertEqual(removed, ["grep", "read"])
        self.assertTrue(forced)
        self.assertEqual(body["tool_choice"]["function"]["name"], "subagent")

    def test_shape_forces_on_a_later_user_turn_too(self):
        pls.FORCE_DELEGATION = True
        body = {
            "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "read a"},
                         {"role": "assistant", "content": "ok"}, {"role": "user", "content": "now grep b"}],
            "tools": [{"type": "function", "function": {"name": "subagent"}}],
        }
        _, _, forced, _ = pls.shape_hybrid(body)
        self.assertTrue(forced)

    def test_shape_respects_an_existing_pin(self):
        pls.FORCE_DELEGATION = True
        body = {"messages": [{"role": "user", "content": "read a"}],
                "tools": [{"type": "function", "function": {"name": "subagent"}}],
                "tool_choice": {"type": "function", "function": {"name": "subagent"}}}
        _, _, forced, _ = pls.shape_hybrid(body)
        self.assertFalse(forced)

    def test_shape_leaves_a_greeting_alone(self):
        pls.FORCE_DELEGATION = True
        body = {"messages": [{"role": "user", "content": "hi"}],
                "tools": [{"type": "function", "function": {"name": "subagent"}}]}
        _, _, forced, _ = pls.shape_hybrid(body)
        self.assertFalse(forced)
        self.assertNotIn("tool_choice", body)

    def test_pii_forces_delegation_even_without_a_work_verb(self):
        pls.FORCE_DELEGATION = True
        body = {"messages": [{"role": "user", "content": "here"}],
                "tools": [{"type": "function", "function": {"name": "subagent"}}]}
        _, _, forced, by_pii = pls.shape_hybrid(body, {"files": {"/w/a.csv": {"email": 1}}, "total": 1})
        self.assertTrue(forced)
        self.assertTrue(by_pii)

    def test_the_delegation_prompt_goes_on_the_system_message_once(self):
        body = {"messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "x"}]}
        self.assertTrue(pls.inject_delegation(body, {"files": {"/w/a.csv": {"email": 1}}, "total": 1}))
        self.assertFalse(pls.inject_delegation(body))
        self.assertIn("cloud half", body["messages"][0]["content"])

    def test_the_child_prompt_can_name_the_disclosure_mode(self):
        mode, messages = pls.take_disclosure(
            [{"role": "user", "content": "disclosure: title\nlist the files"}], "summary")
        self.assertEqual(mode, "title")
        self.assertEqual(messages[0]["content"], "list the files")

    def test_a_prompt_without_a_marker_keeps_the_path_mode(self):
        mode, messages = pls.take_disclosure([{"role": "user", "content": "hi"}], "local_only")
        self.assertEqual(mode, "local_only")
        self.assertEqual(messages[0]["content"], "hi")

    def test_title_mode_returns_one_line(self):
        self.mode("local")
        pls.http.post = _fake_return(completion("First line here\nsecond line\nthird"))
        response = self.post("/local/v1/chat/completions",
                             {"model": "x", "messages": [{"role": "user", "content": "disclosure: title\ngo"}]})
        self.assertEqual(response.json()["choices"][0]["message"]["content"], "First line here")

    def test_summary_redacts_the_values_it_keeps(self):
        self.mode("local")
        blurb = "Row 1 Maya Thornton maya@x.com (555) 010-0101 1988-04-12 " + "pad " * 90
        pls.http.post = _fake_return(completion(blurb))
        response = self.post("/local/v1/chat/completions")
        content = response.json()["choices"][0]["message"]["content"]
        self.assertNotIn("maya@x.com", content)
        self.assertIn("[email]", content)

    def test_disclosure_none_keeps_the_values(self):
        self.mode("local")
        pls.http.post = _fake_return(completion("maya@x.com"))
        response = self.post("/local/v1/chat/completions",
                             {"model": "x", "messages": [{"role": "user", "content": "disclosure: none\ngo"}]})
        self.assertIn("maya@x.com", response.json()["choices"][0]["message"]["content"])

    def test_the_child_is_told_the_disclosure_ceiling(self):
        self.mode("local")
        self.post("/local/v1/chat/completions")
        system = [m for m in self.local_bodies[-1]["messages"] if m["role"] == "system"]
        self.assertEqual(system, [])  # no system message in this fixture
        body = {"messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "x"}]}
        pls.inject_child_rules(body, "title")
        self.assertIn("one line", body["messages"][0]["content"])

    def test_home_is_shortened_in_a_summary(self):
        self.mode("local")
        pls.http.post = _fake_return(completion(str(__import__("pathlib").Path.home()) + "/w/a.csv ok"))
        response = self.post("/local/v1/chat/completions")
        self.assertIn("~/w/a.csv", response.json()["choices"][0]["message"]["content"])

    def test_models_lists_only_the_hybrid_id(self):
        data = self.client.get("/v1/models").json()["data"]
        self.assertEqual([m["id"] for m in data], [pls.MODEL_ID])

    # -- PII guard ---------------------------------------------------------

    def test_the_instruction_names_the_file_the_class_and_the_mode(self):
        text = pls.pii_instruction({"files": {"/w/a.csv": {"email": 2}}, "total": 2}, "local_only")
        self.assertIn("/w/a.csv", text)
        self.assertIn("email", text)
        self.assertIn("disclosure: local_only", text)
        self.assertIn("subagent", text)

    def test_parse_calls_joins_arguments_split_across_chunks(self):
        body = (
            'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":'
            '{"name":"subagent","arguments":"{\\"prompt\\":"}}]}}]}\n\n'
            'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":'
            '{"arguments":"\\"go\\"}"}}]}}]}\n\n'
            "data: [DONE]\n\n"
        ).encode()
        calls = pls.parse_calls(body)
        self.assertEqual(calls[0]["name"], "subagent")
        self.assertEqual(json.loads(calls[0]["arguments"]), {"prompt": "go"})

    def test_a_pii_turn_gets_the_instruction_and_a_subagent_call_is_kept(self):
        sent = []
        full = json.dumps({"description": "Read the file", "prompt": "disclosure: summary\nread it"})
        pls.http.send = _fake_send_sse(sent, ("subagent", full))
        response = self.post("/v1/chat/completions", {
            "stream": True,
            "messages": [{"role": "user", "content": "read /w/a.csv, maya@x.com"}],
            "tools": [{"type": "function", "function": {"name": n}}
                      for n in ("read", "grep", "subagent")],
        })
        self.assertEqual(len(sent), 1)
        self.assertIn("disclosure: local_only", sent[0]["messages"][-1]["content"])
        call = pls.parse_calls(response.content)[0]
        self.assertEqual(json.loads(call["arguments"])["prompt"], "disclosure: summary\nread it")

    def test_an_empty_subagent_call_gets_its_arguments_filled(self):
        sent = []
        pls.http.send = _fake_send_sse(sent, "subagent")
        response = self.post("/v1/chat/completions", {
            "stream": True,
            "messages": [{"role": "user", "content": "read /w/a.csv, maya@x.com"}],
            "tools": [{"type": "function", "function": {"name": "subagent"}}],
        })
        self.assertEqual(len(sent), 1)
        prompt = json.loads(pls.parse_calls(response.content)[0]["arguments"])["prompt"]
        self.assertTrue(prompt.startswith("disclosure: local_only"))
        self.assertIn("/w/a.csv", prompt)

    def test_a_read_call_is_dropped_and_asked_again(self):
        sent = []
        pls.http.send = _fake_send_sse(sent, "read", "subagent")
        response = self.post("/v1/chat/completions", {
            "stream": True,
            "messages": [{"role": "user", "content": "read /w/a.csv, maya@x.com"}],
            "tools": [{"type": "function", "function": {"name": n}}
                      for n in ("read", "subagent")],
        })
        self.assertEqual(len(sent), 2)
        self.assertIn("Wrong: you answered with read", sent[1]["messages"][-1]["content"])
        self.assertIn('"name": "subagent"', response.text)

    def test_the_router_emits_the_call_when_the_model_never_does(self):
        sent = []
        pls.http.send = _fake_send_sse(sent, None, None, None)
        response = self.post("/v1/chat/completions", {
            "stream": True,
            "messages": [{"role": "user", "content": "read /w/a.csv, maya@x.com"}],
            "tools": [{"type": "function", "function": {"name": "subagent"}}],
        })
        self.assertEqual(len(sent), pls.GUARD_RETRIES + 1)
        call = pls.parse_calls(response.content)[0]
        self.assertEqual(call["name"], "subagent")
        prompt = json.loads(call["arguments"])["prompt"]
        self.assertTrue(prompt.startswith("disclosure: local_only"))
        self.assertIn("/w/a.csv", prompt)
        self.assertIn("maya@x.com", prompt)

    def test_a_subagent_call_beside_a_read_is_rejected(self):
        sent = []
        pls.http.send = _fake_send_sse(sent, "read+subagent", "subagent")
        response = self.post("/v1/chat/completions", {
            "stream": True,
            "messages": [{"role": "user", "content": "read /w/a.csv, maya@x.com"}],
            "tools": [{"type": "function", "function": {"name": n}}
                      for n in ("read", "subagent")],
        })
        self.assertEqual(len(sent), 2)
        self.assertEqual(pls.parse_calls(response.content)[0]["name"], "subagent")

    def test_a_turn_without_pii_gets_no_instruction(self):
        sent = []
        pls.http.send = _fake_send_sse(sent, "bash")
        self.post("/v1/chat/completions", {
            "stream": True,
            "messages": [{"role": "user", "content": "read /w/a.csv"}],
            "tools": [{"type": "function", "function": {"name": n}}
                      for n in ("bash", "subagent")],
        })
        self.assertEqual(len(sent), 1)
        self.assertNotIn("disclosure:", sent[0]["messages"][-1]["content"])

    # -- the same guard on a plain work turn -------------------------------

    def test_a_bash_only_reply_is_accepted_on_a_work_turn(self):
        sent = []
        pls.http.send = _fake_send_sse(sent, "bash")
        response = self.post("/v1/chat/completions", {
            "stream": True,
            "messages": [{"role": "user", "content": "read /w/a.csv and run it"}],
            "tools": [{"type": "function", "function": {"name": n}}
                      for n in ("bash", "read", "subagent")],
        })
        self.assertEqual(len(sent), 1)
        self.assertEqual(pls.parse_calls(response.content)[0]["name"], "bash")

    def test_a_read_call_is_rejected_on_a_work_turn(self):
        sent = []
        pls.http.send = _fake_send_sse(sent, "read", "subagent")
        response = self.post("/v1/chat/completions", {
            "stream": True,
            "messages": [{"role": "user", "content": "read /w/a.csv"}],
            "tools": [{"type": "function", "function": {"name": n}}
                      for n in ("bash", "read", "subagent")],
        })
        self.assertEqual(len(sent), 2)
        self.assertIn("Wrong: you answered with read", sent[1]["messages"][-1]["content"])
        self.assertEqual(pls.parse_calls(response.content)[0]["name"], "subagent")

    def test_a_stubborn_work_turn_is_synthesised_in_summary_mode(self):
        sent = []
        pls.http.send = _fake_send_sse(sent, "read")
        response = self.post("/v1/chat/completions", {
            "stream": True,
            "messages": [{"role": "user", "content": "read /w/a.csv"}],
            "tools": [{"type": "function", "function": {"name": n}}
                      for n in ("read", "subagent")],
        })
        self.assertEqual(len(sent), pls.GUARD_RETRIES + 1)
        prompt = json.loads(pls.parse_calls(response.content)[0]["arguments"])["prompt"]
        self.assertTrue(prompt.startswith("disclosure: summary"))
        self.assertIn("/w/a.csv", prompt)


    def socket(self, sub):
        """A real unix socket answering like wally, so the parser is exercised."""
        path = str(self.dir / "entitlement.sock")
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
        server = socket_mod.socket(socket_mod.AF_UNIX, socket_mod.SOCK_STREAM)
        server.bind(path)
        server.listen(4)

        def serve():
            while True:
                try:
                    conn, _ = server.accept()
                except OSError:
                    return
                conn.recv(256)
                body = json.dumps({"sub": sub}).encode()
                conn.sendall(b"HTTP/1.1 200 OK\r\ncontent-length: " + str(len(body)).encode() + b"\r\n\r\n" + body)
                conn.close()

        import threading

        threading.Thread(target=serve, daemon=True).start()
        return path


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
    """Answer successive cloud sends with these tool names; the last repeats."""
    queue = list(replies)

    async def send(request, stream=False):
        sent.append(json.loads(request.content))
        reply = queue.pop(0) if len(queue) > 1 else (queue[0] if queue else None)
        if isinstance(reply, tuple):
            return FakeStream(_sse(*reply))
        return FakeStream(_sse(reply))

    return send


def _fake_return(payload):
    async def post(url, json=None, headers=None):
        return FakeResponse(payload)

    return post


def _fake_send(payload):
    async def send(request, stream=False):
        return FakeResponse(payload)

    return send


if __name__ == "__main__":
    unittest.main()
