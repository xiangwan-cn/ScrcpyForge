# -*- mode: python ; coding: utf-8 -*-

block_cipher = None

a = Analysis(
    ["scrcpy_script/main.py"],
    pathex=[],
    binaries=[],
    datas=[],
    hiddenimports=[
        "av", "av.codec", "av.container", "av.stream",
        "av.packet", "av.format", "av.bitstream",
        "cv2", "numpy", "numpy.core._methods",
        "dearpygui", "dearpygui._dearpygui",
        "watchdog", "watchdog.observers", "watchdog.events",
    ],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[],
    win_no_prefer_redirects=False,
    win_private_assemblies=False,
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
    name="scrcpy_script",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=True,
    upx_exclude=[],
    runtime_tmpdir=None,
    console=True,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
)
