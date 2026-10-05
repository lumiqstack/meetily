"""Compile the production native guard with a throwing backend and cross it from Rust."""
from pathlib import Path
import subprocess, tempfile, sys
root = Path(__file__).resolve().parents[2]
with tempfile.TemporaryDirectory(prefix="meetily-native-test-") as folder:
    out = Path(folder)
    objects = []
    for i, source in enumerate([root/'vendor/whisper-rs/native/guard.cpp', Path(__file__).with_name('throwing_backend.cpp')]):
        obj = out/f'{i}.o'; objects.append(str(obj))
        subprocess.run(['c++', '-std=c++11', '-I'+str(root/'vendor/whisper-rs/native'), '-c', str(source), '-o', str(obj)], check=True)
    subprocess.run(['ar', 'rcs', str(out/'libguard_test.a'), *objects], check=True)
    subprocess.run(['rustc', '--edition=2021', '--test', str(Path(__file__).with_name('boundary.rs')), '-L', str(out), '-l', 'static=guard_test', '-l', 'c++' if sys.platform == 'darwin' else 'stdc++', '-o', str(out/'boundary')], check=True)
    subprocess.run([str(out/'boundary')], check=True)
