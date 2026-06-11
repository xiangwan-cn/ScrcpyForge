"""Launch native scrcpy windows per device."""
import os
import signal
import subprocess
from typing import Optional

from scrcpy_script.config import resolve_scrcpy_path


class ScrcpyLauncher:
    def __init__(self, max_size: int = 1280) -> None:
        self._processes: dict[str, subprocess.Popen] = {}
        self._max_size = max_size

    def launch(self, serial: str, max_size: Optional[int] = None) -> None:
        if serial in self._processes:
            proc = self._processes[serial]
            if proc.poll() is None:
                return

        size = max_size or self._max_size
        kwargs = {}
        if os.name != "nt":
            kwargs["preexec_fn"] = os.setsid
        else:
            kwargs["creationflags"] = 0x08000000

        proc = subprocess.Popen(
            [resolve_scrcpy_path(), "-s", serial, "-m", str(size)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            **kwargs,
        )
        self._processes[serial] = proc

    def close(self, serial: str) -> None:
        proc = self._processes.pop(serial, None)
        if proc and proc.poll() is None:
            try:
                if os.name == "nt":
                    proc.terminate()
                else:
                    os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
                proc.wait(timeout=5)
            except (subprocess.TimeoutExpired, ProcessLookupError, OSError):
                proc.kill()

    def close_all(self) -> None:
        for serial in list(self._processes.keys()):
            self.close(serial)

    def is_running(self, serial: str) -> bool:
        proc = self._processes.get(serial)
        return proc is not None and proc.poll() is None
