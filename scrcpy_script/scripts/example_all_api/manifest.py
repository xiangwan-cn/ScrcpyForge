"""Script manifest — must export script(api)."""
from importlib.machinery import SourceFileLoader
from pathlib import Path

NAME = "This is a example script" 

_script_path = Path(__file__).parent / "script.py"
_loader = SourceFileLoader("script", str(_script_path))
_mod = _loader.load_module()
script = _mod.script
