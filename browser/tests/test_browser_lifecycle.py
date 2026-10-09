"""When the browser is gone, the command should stop. No Chrome in these tests."""

from wally_browser.run import browser_should_stop


def test_a_dead_chrome_process_stops_immediately():
    stop, seen, polls = browser_should_stop(False, 2, True, 0)
    assert stop is True
    assert seen is True
    assert polls == 0


def test_pages_have_to_be_gone_for_several_polls():
    stop, seen, polls = browser_should_stop(True, 0, True, 0)
    assert stop is False
    assert seen is True
    assert polls == 1
    stop, seen, polls = browser_should_stop(True, 0, seen, polls)
    assert stop is False
    stop, seen, polls = browser_should_stop(True, 0, seen, polls)
    assert stop is True
    assert polls == 3


def test_a_page_coming_back_resets_the_empty_polls():
    stop, seen, polls = browser_should_stop(True, 0, True, 0)
    assert stop is False
    assert polls == 1
    stop, seen, polls = browser_should_stop(True, 1, seen, polls)
    assert stop is False
    assert polls == 0
    assert seen is True


def test_startup_with_no_pages_yet_does_not_stop():
    stop, seen, polls = browser_should_stop(True, 0, False, 0)
    assert stop is False
    assert seen is False


def test_an_unreadable_page_list_before_any_page_does_not_stop():
    stop, seen, polls = browser_should_stop(None, None, False, 0)
    assert stop is False
    assert seen is False
    assert polls == 0


def test_an_unreadable_page_list_after_a_page_counts_as_empty():
    stop, seen, polls = browser_should_stop(None, None, True, 2)
    assert stop is True
    assert seen is True
