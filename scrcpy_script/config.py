"""Configuration reader for scrcpy_config.conf (key=value format)."""
import configparser
from typing import Optional


class Config:
    def __init__(self, path: Optional[str] = None) -> None:
        self._parser = configparser.ConfigParser()
        self._parser.optionxform = str
        self._parser["scrcpy"] = {}
        if path:
            self.load(path)

    def load(self, path: str) -> None:
        with open(path, encoding="utf-8") as f:
            content = f.read()
        self._parser.read_string("[scrcpy]\n" + content)

    def get(self, key: str, default: str = "") -> str:
        return self._parser.get("scrcpy", key, fallback=default)

    def get_int(self, key: str, default: int = 0) -> int:
        try:
            return self._parser.getint("scrcpy", key)
        except (configparser.NoOptionError, ValueError):
            return default

    def get_bool(self, key: str, default: bool = False) -> bool:
        try:
            return self._parser.getboolean("scrcpy", key)
        except (configparser.NoOptionError, ValueError):
            return default

    @property
    def port_start(self) -> int:
        return self.get_int("port_start", 27183)

    @property
    def port_end(self) -> int:
        return self.get_int("port_end", 27282)

    @property
    def max_devices(self) -> int:
        return self.get_int("max_devices", 10)

    @property
    def scripts_dir(self) -> str:
        return self.get("scripts_dir", "scrcpy_script/scripts")

    @property
    def templates_dir(self) -> str:
        return self.get("templates_dir", "templates")

    @property
    def video_codec(self) -> str:
        return self.get("video_codec", "h264")

    @property
    def video_encoder(self) -> str:
        return self.get("video_encoder", "")

    @property
    def max_size(self) -> int:
        return self.get_int("max_size", 1280)

    @property
    def bit_rate(self) -> int:
        return self.get_int("bit_rate", 8000000)

    @property
    def max_fps(self) -> int:
        return self.get_int("max_fps", 60)

    @property
    def jar_path(self) -> str:
        return self.get("server_jar", "scrcpy-server-v4.0.jar")
