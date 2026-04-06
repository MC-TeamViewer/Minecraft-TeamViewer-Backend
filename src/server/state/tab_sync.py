from typing import Optional


def normalize_tab_uuid(value) -> Optional[str]:
    text = str(value or "").strip().lower()
    if len(text) != 36:
        return None
    return text


def normalize_tab_name(value) -> Optional[str]:
    if not isinstance(value, str):
        return None
    text = value.strip()
    if not text:
        return None
    return text[:64]
