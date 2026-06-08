# -*- mode: python ; coding: utf-8 -*-
from PyInstaller.utils.hooks import collect_submodules

block_cipher = None

av_imports = collect_submodules("av")

a = Analysis(
    ["scrcpy_script/main.py"],
    pathex=[],
    binaries=[],
    datas=[],
    hiddenimports=[
        "cv2", "numpy", "numpy.core._methods",
        "dearpygui", "dearpygui._dearpygui",
        "watchdog", "watchdog.observers", "watchdog.events",
    ] + av_imports,
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[],
    cipher=block_cipher,
    noarchive=False,
)

pyz = PYZ(a.pure, a.zipped_data, cipher=block_cipher)

exe = EXE(
    pyz,
    a.scripts,
    a.binaries,
    a.zipfiles,
    a.datas,
    [],
    name="scrcpy_forge",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=True,
    console=True,
)
