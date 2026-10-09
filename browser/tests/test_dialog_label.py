"""A class that merely contains 'dialog' or 'overlay' is not a dialog."""

from wally_browser.elements import marks_dialog


def test_a_substring_in_a_class_name_is_not_a_dialog():
    assert marks_dialog("DIV", {"class": "vector-overlay-search"}) is False
    assert marks_dialog("DIV", {"class": "mw-search-dialog-hint"}) is False


def test_a_real_dialog_class_or_role_is_a_dialog():
    assert marks_dialog("DIV", {"class": "modal open"}) is True
    assert marks_dialog("DIV", {"role": "dialog"}) is True
    assert marks_dialog("DIALOG", {}) is True
