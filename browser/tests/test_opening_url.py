from wally_browser.text import parse_opening_url


def test_a_url_is_kept_and_ask_is_not():
    assert parse_opening_url("https://en.wikipedia.org/wiki/Bengaluru") == (
        "https://en.wikipedia.org/wiki/Bengaluru"
    )
    assert parse_opening_url("ASK") is None
    assert parse_opening_url("I do not know the site") is None
