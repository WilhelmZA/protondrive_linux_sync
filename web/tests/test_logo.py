"""Committed logo matches assets/logo.png byte for byte."""

from tests.conftest import LOGO_SRC, LOGO_WEB


def test_logo_matches_assets() -> None:
    assert LOGO_SRC.is_file()
    assert LOGO_WEB.is_file()
    assert LOGO_WEB.read_bytes() == LOGO_SRC.read_bytes()
