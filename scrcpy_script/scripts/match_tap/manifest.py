"""Script manifest — must export script(api)."""
NAME = "模板匹配点击"  # UI 显示名称

from importlib.machinery import SourceFileLoader
from pathlib import Path

_script_path = Path(__file__).parent / "script.py"
_loader = SourceFileLoader("script", str(_script_path))
_mod = _loader.load_module()
script = _mod.script
