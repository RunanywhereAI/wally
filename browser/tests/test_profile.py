"""The traveller profile refuses payment data and unknown keys."""

import pytest

from wally_browser.profile import ProfileError, load, parse


def test_a_traveller_profile_loads(tmp_path):
    path = tmp_path / "traveller.toml"
    path.write_text('[traveller]\nfirst_name = "Asha"\nlast_name = "Rao"\ndate_of_birth = "1990-04-12"\n')
    profile = load(path)
    assert profile.get("first_name") == "Asha"
    assert profile.get("full_name") == "Asha Rao"
    assert profile.keys()[:2] == ["first_name", "last_name"]


@pytest.mark.parametrize("key", ["card_number", "cvv", "card_expiry", "upi_id", "upi_pin", "bank_account",
                                 "ifsc", "otp", "netbanking_password", "billing_card"])
def test_payment_keys_are_refused_by_name(key):
    with pytest.raises(ProfileError, match="payment or account data"):
        parse({"traveller": {key: "x"}})


def test_unknown_keys_are_refused():
    with pytest.raises(ProfileError, match="unknown key 'frist_name'"):
        parse({"traveller": {"frist_name": "Asha"}})


def test_dates_are_checked():
    with pytest.raises(ProfileError, match="YYYY-MM-DD"):
        parse({"traveller": {"date_of_birth": "12/04/1990"}})


def test_a_missing_default_profile_is_empty_but_a_named_one_is_an_error(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path))
    assert load().keys() == []
    with pytest.raises(ProfileError, match="no such file"):
        load(tmp_path / "missing.toml")
