"""Compile the production native guard with a throwing backend and cross it from Rust.

Uses MSVC (cl /EHsc, lib) on Windows, matching how vendor/whisper-rs builds the
guard there, and the Unix C++ toolchain (c++, ar) elsewhere. On Windows, run it
from a Visual Studio developer shell so cl.exe and lib.exe are on PATH.
"""
from pathlib import Path
import subprocess, tempfile, sys
root = Path(__file__).resolve().parents[2]
native = root/'vendor/whisper-rs/native'
sources = [native/'guard.cpp', Path(__file__).with_name('throwing_backend.cpp')]
msvc = sys.platform == 'win32'
with tempfile.TemporaryDirectory(prefix="meetily-native-test-") as folder:
    out = Path(folder)
    objects = []
    for i, source in enumerate(sources):
        if msvc:
            obj = out/f'{i}.obj'
            subprocess.run(['cl', '/nologo', '/EHsc', '/MD', '/I'+str(native), '/c', str(source), '/Fo'+str(obj)], check=True)
        else:
            obj = out/f'{i}.o'
            subprocess.run(['c++', '-std=c++11', '-I'+str(native), '-c', str(source), '-o', str(obj)], check=True)
        objects.append(str(obj))
    if msvc:
        subprocess.run(['lib', '/nologo', '/OUT:'+str(out/'guard_test.lib'), *objects], check=True)
        cxx = []  # the MSVC C++ runtime is linked through /MD defaults
    else:
        subprocess.run(['ar', 'rcs', str(out/'libguard_test.a'), *objects], check=True)
        cxx = ['-l', 'c++' if sys.platform == 'darwin' else 'stdc++']
    exe = out/('boundary.exe' if msvc else 'boundary')
    subprocess.run(['rustc', '--edition=2021', '--test', str(Path(__file__).with_name('boundary.rs')), '-L', str(out), '-l', 'static=guard_test', *cxx, '-o', str(exe)], check=True)
    subprocess.run([str(exe)], check=True)
