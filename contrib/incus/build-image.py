#!/usr/bin/env python3
import argparse
import hashlib
import io
import os
import sys
import tarfile
import time
import urllib.request
from typing import Optional

ALPINE = "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases"
ARCHES = {"amd64": "x86_64", "arm64": "aarch64"}
DATA = "/var/lib/loom-host"

INITTAB = """::sysinit:/usr/local/lib/loom-host/init
::respawn:/usr/local/lib/loom-host/run
"""

INIT = f"""#!/bin/sh
mkdir -p {DATA}/config {DATA}/data {DATA}/state
ip link set lo up
udhcpc -b -i eth0 -x hostname:"$(hostname)" >/dev/console 2>&1
"""

RUN = f"""#!/bin/sh
export HOME=/root
cd {DATA}
/usr/local/bin/loom-host serve --wait >/dev/console 2>&1
sleep 5
"""

HOSTS = """127.0.0.1\tlocalhost
::1\tlocalhost ip6-localhost ip6-loopback
"""

Entry = tuple[tarfile.TarInfo, Optional[bytes]]


def fetch(url: str) -> bytes:
    with urllib.request.urlopen(url, timeout=120) as response:
        return response.read()


def alpine_rootfs(arch: str) -> tuple[bytes, str]:
    index = fetch(f"{ALPINE}/{arch}/latest-releases.yaml").decode()
    for block in index.split("\n-\n"):
        fields = {}
        for line in block.splitlines():
            key, sep, value = line.strip().partition(": ")
            if sep:
                fields[key] = value.strip()
        if fields.get("flavor") != "alpine-minirootfs":
            continue
        data = fetch(f"{ALPINE}/{arch}/{fields['file']}")
        if hashlib.sha256(data).hexdigest() != fields["sha256"]:
            sys.exit(f"the checksum of {fields['file']} does not match")
        return data, fields["version"]
    sys.exit(f"no minirootfs for {arch} in latest-releases.yaml")


def entry(name: str, kind: bytes, mode: int, now: int, data: Optional[bytes] = None, link: str = "") -> Entry:
    info = tarfile.TarInfo(name)
    info.type = kind
    info.mode = mode
    info.mtime = now
    info.linkname = link
    if data is not None:
        info.size = len(data)
    return info, data


def extra_entries(bin_dir: str, now: int) -> list[Entry]:
    def binary(name: str) -> bytes:
        with open(os.path.join(bin_dir, name), "rb") as f:
            return f.read()

    return [
        entry("etc/inittab", tarfile.REGTYPE, 0o644, now, INITTAB.encode()),
        entry("etc/hosts", tarfile.REGTYPE, 0o644, now, HOSTS.encode()),
        entry("etc/hostname", tarfile.REGTYPE, 0o644, now, b"loom-host\n"),
        entry("usr/local/bin/loom-host", tarfile.REGTYPE, 0o755, now, binary("loom-host")),
        entry("usr/local/bin/loom-agent", tarfile.REGTYPE, 0o755, now, binary("loom-agent")),
        entry("usr/local/lib/loom-host", tarfile.DIRTYPE, 0o755, now),
        entry("usr/local/lib/loom-host/init", tarfile.REGTYPE, 0o755, now, INIT.encode()),
        entry("usr/local/lib/loom-host/run", tarfile.REGTYPE, 0o755, now, RUN.encode()),
        entry("var/lib/loom-host", tarfile.DIRTYPE, 0o755, now),
        entry("root/.config", tarfile.SYMTYPE, 0o777, now, link=f"{DATA}/config"),
        entry("root/.local", tarfile.DIRTYPE, 0o755, now),
        entry("root/.local/share", tarfile.SYMTYPE, 0o777, now, link=f"{DATA}/data"),
        entry("root/.local/state", tarfile.SYMTYPE, 0o777, now, link=f"{DATA}/state"),
    ]


def write_rootfs(path: str, base: bytes, extras: list[Entry]) -> None:
    replaced = {info.name for info, _ in extras}
    with tarfile.open(fileobj=io.BytesIO(base), mode="r:*") as source, tarfile.open(path, "w:xz", format=tarfile.PAX_FORMAT) as out:
        for member in source:
            name = member.name.removeprefix("./").rstrip("/")
            if name in replaced:
                continue
            data = source.extractfile(member).read() if member.isfile() else None
            out.addfile(member, io.BytesIO(data) if data is not None else None)
        for info, data in extras:
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            out.addfile(info, io.BytesIO(data) if data is not None else None)


def write_metadata(path: str, arch: str, version: str, alpine: str, now: int) -> None:
    text = (
        f"architecture: {ARCHES[arch]}\n"
        f"creation_date: {now}\n"
        "properties:\n"
        f"  description: loom-host {version} (Alpine Linux {alpine})\n"
        "  os: alpine\n"
        f"  release: \"{alpine}\"\n"
        f"  architecture: {arch}\n"
        f"  version: \"{version}\"\n"
    ).encode()
    with tarfile.open(path, "w:xz") as out:
        info, data = entry("metadata.yaml", tarfile.REGTYPE, 0o644, now, text)
        out.addfile(info, io.BytesIO(data))


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Build the Incus image of loom-host as a metadata tarball and a rootfs tarball."
    )
    parser.add_argument("--arch", choices=sorted(ARCHES), required=True)
    parser.add_argument("--bin-dir", required=True, help="the directory with loom-host and loom-agent for this architecture")
    parser.add_argument("--output", required=True, help="the directory of the two tarballs")
    parser.add_argument("--version", default="dev")
    parser.add_argument("--base", help="an Alpine minirootfs tarball; the default downloads the latest stable release")
    args = parser.parse_args()

    if args.base:
        with open(args.base, "rb") as f:
            base, alpine = f.read(), "custom"
    else:
        base, alpine = alpine_rootfs(ARCHES[args.arch])
    now = int(time.time())
    os.makedirs(args.output, exist_ok=True)
    prefix = os.path.join(args.output, f"loom-host-{args.arch}")
    write_rootfs(f"{prefix}.rootfs.tar.xz", base, extra_entries(args.bin_dir, now))
    write_metadata(f"{prefix}.metadata.tar.xz", args.arch, args.version, alpine, now)
    print(f"{prefix}.metadata.tar.xz")
    print(f"{prefix}.rootfs.tar.xz")


if __name__ == "__main__":
    main()
