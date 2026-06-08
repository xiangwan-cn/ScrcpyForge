# -*- mode: python ; coding: utf-8 -*-
from PyInstaller.utils.hooks import collect_submodules, collect_data_files

block_cipher = None

av_imports = collect_submodules("av")
dpg_imports = collect_submodules("dearpygui")
dpg_datas = collect_data_files("dearpygui")

a = Analysis(
    ["scrcpy_script/main.py"],
    pathex=[],
    binaries=[],
    datas=dpg_datas,
    hiddenimports=[
        "cv2", "numpy", "numpy.core._methods",
        "watchdog", "watchdog.observers", "watchdog.events",
    ] + av_imports + dpg_imports,
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
