from __future__ import annotations

from pathlib import Path


BACKEND_ROOT = Path(__file__).resolve().parents[2]
ADMIN_UI_DIST_DIR = BACKEND_ROOT / "admin-ui" / "dist"
ADMIN_UI_INDEX_PATH = ADMIN_UI_DIST_DIR / "index.html"
ADMIN_UI_ASSETS_DIR = ADMIN_UI_DIST_DIR / "assets"


def admin_ui_ready() -> bool:
    return ADMIN_UI_INDEX_PATH.is_file()


def resolve_admin_asset_path(asset_path: str) -> Path | None:
    if not asset_path:
        return None

    dist_root = ADMIN_UI_DIST_DIR.resolve()
    normalized_paths = [dist_root / asset_path]

    if asset_path.startswith("assets/"):
        normalized_paths.append(ADMIN_UI_ASSETS_DIR.resolve() / asset_path.removeprefix("assets/"))
    else:
        normalized_paths.append(ADMIN_UI_ASSETS_DIR.resolve() / asset_path)

    for candidate in normalized_paths:
        try:
            candidate.relative_to(dist_root)
        except ValueError:
            continue
        if candidate.is_file():
            return candidate

    return None
