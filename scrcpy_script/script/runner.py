"""Per-device script execution thread."""
import threading
from types import ModuleType
from typing import Optional


class ScriptRunner:
    def __init__(self, api=None) -> None:
        self._api = api
        self._stop_event = threading.Event()
        self._thread: Optional[threading.Thread] = None
        self._running = False

    def set_api(self, api) -> None:
        self._api = api

    def run(self, script_module: ModuleType) -> None:
        if self._running:
            return
        self._stop_event.clear()
        self._running = True
        api = self._api

        def wrapper() -> None:
            api.log("Script started")
            try:
                script_module.script(api)
            except StopIteration:
                pass  # Normal stop
            except Exception as e:
                api.warn(f"Script error: {e}")
            finally:
                api.log("Script stopped")

        self._thread = threading.Thread(target=wrapper, daemon=True)
        self._thread.start()

    def stop(self, timeout: float = 5.0) -> None:
        if not self._running:
            return
        self._stop_event.set()
        self._running = False
        if self._thread and self._thread.is_alive():
            self._thread.join(timeout=timeout)

    @property
    def is_running(self) -> bool:
        return self._running

    @property
    def stop_event(self) -> threading.Event:
        return self._stop_event
