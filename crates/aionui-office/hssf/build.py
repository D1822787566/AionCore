"""Build the pinned, offline Windows HSSF runtime; never downloads at app runtime."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import urllib.request
import zipfile

HERE = Path(__file__).resolve().parent


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def build(jdk, cache, output):
    lock = json.loads((HERE / "dependencies.lock.json").read_text("utf8"))
    release = (jdk / "release").read_text("utf8")
    versions = (f'JAVA_RUNTIME_VERSION="{lock["java_version"]}"',
                f'JAVA_RUNTIME_VERSION="{lock["java_version"]}-LTS"')
    if not any(version in release for version in versions) or 'OS_ARCH="x86_64"' not in release:
        raise RuntimeError("JDK runtime version does not match dependencies.lock.json")
    java = jdk / "bin/java.exe"
    if not java.is_file():
        raise RuntimeError("this bundle targets Windows x64")
    cache.mkdir(parents=True, exist_ok=True)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="hssf-build-", dir=output.parent) as temp:
        staging = Path(temp) / "bundle"
        (staging / "lib").mkdir(parents=True)
        (staging / "classes").mkdir()
        (staging / "licenses").mkdir()
        for item in lock["jars"]:
            name = item["url"].rsplit("/", 1)[-1]
            jar = cache / name
            if not jar.exists():
                with urllib.request.urlopen(item["url"], timeout=60) as response:
                    jar.write_bytes(response.read())
            if sha(jar) != item["sha256"]:
                raise RuntimeError("dependency checksum mismatch: " + name)
            shutil.copy2(jar, staging / "lib" / name)
            with zipfile.ZipFile(jar) as archive:
                for info in archive.infolist():
                    leaf = info.filename.rsplit("/", 1)[-1]
                    if not info.is_dir() and leaf.upper().startswith(("LICENSE", "NOTICE")):
                        (staging / "licenses" / (name + "-" + leaf)).write_bytes(archive.read(info))
        subprocess.run([str(jdk / "bin/javac.exe"), "-encoding", "UTF-8",
                        "-cp", str(staging / "lib/*"), "-d", str(staging / "classes"),
                        str(HERE / "HssfBridge.java")], check=True)
        subprocess.run([str(jdk / "bin/jlink.exe"), "--add-modules",
                        "java.base,java.desktop,java.logging,java.xml",
                        "--strip-debug", "--no-header-files", "--no-man-pages",
                        "--output", str(staging / "runtime")], check=True)
        shutil.copy2(jdk / "NOTICE", staging / "licenses" / "NOTICE.java.txt")
        shutil.copy2(HERE / "dependencies.lock.json", staging / "dependencies.lock.json")
        files = {str(p.relative_to(staging)).replace("\\", "/"): sha(p)
                 for p in sorted(staging.rglob("*")) if p.is_file()}
        manifest = {"schema_version": 1, "runtime_key": "win32-x64",
                    "poi_version": lock["poi_version"], "java_version": lock["java_version"],
                    "bridge_source_sha256": sha(HERE / "HssfBridge.java"), "files": files}
        (staging / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf8")
        if output.exists():
            raise RuntimeError("output already exists; select a fresh bundle directory")
        shutil.copytree(staging, output)
    print(f"HSSF_BUNDLE_BUILT files={len(files)} poi={lock['poi_version']} java={lock['java_version']}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--jdk", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    build(args.jdk.resolve(), args.cache.resolve(), args.output.resolve())
