"""Decision failures stay within the policy's structured error boundary."""

import httpx
import pytest

from wally_browser.decisions import DecisionClient, DecisionError, Question


@pytest.mark.parametrize("error_type", [httpx.ConnectError, httpx.ReadTimeout, httpx.RemoteProtocolError])
@pytest.mark.parametrize("recovers", [True, False])
def test_transport_failures_retry_and_report_exhaustion(error_type, recovers):
    calls = []
    waits = []
    failure = error_type("connection failed")

    def handler(request):
        calls.append(request)
        if recovers and len(calls) == 3:
            return httpx.Response(200, json={"answers": {"q": {"probabilities": {"yes": 1.0}}}})
        raise failure

    client = DecisionClient("https://example.test/v1", "k", "eve",
                            transport=httpx.MockTransport(handler), sleep=waits.append)
    try:
        if recovers:
            result = client.ask("x", [Question("q", "yes_no", "ok?")])
            assert result.attempts == 3
            assert result.answers["q"].p("yes") == 1.0
        else:
            with pytest.raises(DecisionError) as raised:
                client.ask("x", [Question("q", "yes_no", "ok?")])
            assert raised.value.__cause__ is failure
        assert len(calls) == 3
        assert waits == [1, 1]
    finally:
        client.close()


def test_invalid_json_is_a_decision_error():
    client = DecisionClient("https://example.test/v1", "k", "eve", transport=httpx.MockTransport(
        lambda request: httpx.Response(200, text="not JSON")))
    try:
        with pytest.raises(DecisionError, match="not valid JSON") as raised:
            client.ask("x", [Question("q", "yes_no", "ok?")])
        assert isinstance(raised.value.__cause__, ValueError)
        assert raised.value.status == 200
    finally:
        client.close()
