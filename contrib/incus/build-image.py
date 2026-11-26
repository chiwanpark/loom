#!/usr/bin/env python3
import argparse
import base64
import hashlib
import io
import os
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request
import zlib
from typing import Optional

ALPINE = "https://dl-cdn.alpinelinux.org/alpine/latest-stable/releases"
ARCHES = {"amd64": "x86_64", "arm64": "aarch64"}
LOG_LIMIT = 10 * 1024 * 1024

COMPONENTS = {
    "host": {
        "name": "loom-host",
        "binaries": ["loom-host", "loom-agent"],
        "packages": ["dropbear"],
    },
    "oracle": {
        "name": "loom-oracle",
        "binaries": ["loom-oracle"],
        "packages": [],
    },
}

HOSTS = """127.0.0.1\tlocalhost
::1\tlocalhost ip6-localhost ip6-loopback
"""

Entry = tuple[tarfile.TarInfo, Optional[bytes]]


def inittab(lib: str, ssh: bool) -> str:
    lines = [f"::sysinit:{lib}/init", f"::respawn:{lib}/run", f"::respawn:{lib}/rotate"]
    if ssh:
        lines.append(f"::respawn:{lib}/sshd")
    return "\n".join(lines) + "\n"


def init_script() -> str:
    return """#!/bin/sh
ip link set lo up
udhcpc -b -i eth0 -x hostname:"$(hostname)" >/dev/console 2>&1
"""


def volume_check(data: str) -> str:
    return f"""if ! mountpoint -q {data}; then
    echo "$0: waiting: attach a volume at {data}" >/dev/console
    sleep 30
    exit 0
fi
mkdir -p {data}/config {data}/data {data}/state {data}/log
"""


def run_script(name: str, data: str) -> str:
    return f"""#!/bin/sh
export HOME=/root NO_COLOR=1
{volume_check(data)}cd {data} || exit 1
/usr/local/bin/{name} serve --wait >>{data}/log/{name}.log 2>&1
sleep 5
"""


def rotate_script(data: str) -> str:
    return f"""#!/bin/sh
while sleep 300; do
    for log in {data}/log/*.log; do
        [ -f "$log" ] || continue
        if [ "$(wc -c <"$log")" -gt {LOG_LIMIT} ]; then
            cp "$log" "$log.1" && : >"$log"
        fi
    done
done
"""


def sshd_script(name: str, data: str) -> str:
    return f"""#!/bin/sh
export HOME=/root
{volume_check(data)}mkdir -p {data}/ssh
chmod go-w {data} && chmod 700 {data}/ssh
key={data}/ssh/dropbear_ed25519_host_key
[ -f "$key" ] || dropbearkey -t ed25519 -f "$key" >>{data}/log/sshd.log 2>&1
exec dropbear -F -E -s -j -k -p 22 -r "$key" -D {data}/ssh \\
    -c "/usr/local/bin/{name} ssh-command" >>{data}/log/sshd.log 2>&1
"""


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


def gzip_members(data: bytes) -> list[tuple[bytes, bytes]]:
    members = []
    rest = data
    while rest:
        stream = zlib.decompressobj(wbits=31)
        plain = stream.decompress(rest)
        if not stream.eof:
            sys.exit("a gzip stream of a package ends too early")
        used = len(rest) - len(stream.unused_data)
        members.append((rest[:used], plain))
        rest = stream.unused_data
    return members


def tar_files(plain: bytes) -> dict[str, bytes]:
    files = {}
    with tarfile.open(fileobj=io.BytesIO(plain), mode="r:") as archive:
        for member in archive:
            if member.isfile():
                files[member.name] = archive.extractfile(member).read()
    return files


def rootfs_files(base: bytes, names: list[str]) -> dict[str, bytes]:
    found = {}
    with tarfile.open(fileobj=io.BytesIO(base), mode="r:*") as archive:
        for member in archive:
            name = member.name.removeprefix("./")
            if member.isfile() and any(name == n or name.startswith(n) for n in names):
                found[name] = archive.extractfile(member).read()
    return found


def verify_signature(members: list[tuple[bytes, bytes]], keys: dict[str, bytes]) -> None:
    signature = tar_files(members[0][1])
    for name, sig in signature.items():
        for prefix, digest in ((".SIGN.RSA256.", "-sha256"), (".SIGN.RSA.", "-sha1")):
            if not name.startswith(prefix):
                continue
            key = keys.get(name.removeprefix(prefix))
            if key is None:
                continue
            with tempfile.TemporaryDirectory() as tmp:
                paths = {}
                for part, data in (("key", key), ("sig", sig), ("data", members[1][0])):
                    paths[part] = os.path.join(tmp, part)
                    with open(paths[part], "wb") as f:
                        f.write(data)
                result = subprocess.run(
                    ["openssl", "dgst", digest, "-verify", paths["key"], "-signature", paths["sig"], paths["data"]],
                    capture_output=True,
                )
            if result.returncode == 0:
                return
            sys.exit(f"the signature {name} does not match")
    sys.exit("the package index has no signature of a known key")


def parse_index(text: str) -> list[dict[str, str]]:
    packages = []
    for block in text.strip().split("\n\n"):
        fields = {}
        for line in block.splitlines():
            key, sep, value = line.partition(":")
            if sep:
                fields[key] = value
        packages.append(fields)
    return packages


def dependency_name(dep: str) -> str:
    for sep in "<>=~":
        dep = dep.split(sep, 1)[0]
    return dep


def resolve(wanted: list[str], index: list[dict[str, str]], installed: list[dict[str, str]]) -> list[dict[str, str]]:
    def provides(package: dict[str, str]) -> list[str]:
        return [package["P"]] + [dependency_name(p) for p in package.get("p", "").split()]

    present = {name for package in installed for name in provides(package)}
    providers: dict[str, dict[str, str]] = {}
    for package in index:
        for name in provides(package):
            providers.setdefault(name, package)
    chosen: list[dict[str, str]] = []
    pending = list(wanted)
    while pending:
        dep = pending.pop()
        if dep.startswith("!"):
            continue
        name = dependency_name(dep)
        if name in present:
            continue
        package = providers.get(name)
        if package is None:
            sys.exit(f"no package provides {name}")
        chosen.append(package)
        present.update(provides(package))
        pending.extend(package.get("D", "").split())
    return chosen


def alpine_packages(base: bytes, wanted: list[str], now: int) -> list[Entry]:
    if not wanted:
        return []
    files = rootfs_files(base, ["etc/apk/repositories", "etc/apk/arch", "etc/apk/keys/", "lib/apk/db/installed"])
    arch = files["etc/apk/arch"].decode().strip()
    repository = next(
        line.strip()
        for line in files["etc/apk/repositories"].decode().splitlines()
        if line.strip().endswith("/main")
    )
    keys = {name.removeprefix("etc/apk/keys/"): data for name, data in files.items() if name.startswith("etc/apk/keys/")}
    members = gzip_members(fetch(f"{repository}/{arch}/APKINDEX.tar.gz"))
    verify_signature(members, keys)
    index = parse_index(tar_files(members[1][1])["APKINDEX"].decode())
    installed = parse_index(files["lib/apk/db/installed"].decode())
    entries: list[Entry] = []
    for package in resolve(wanted, index, installed):
        data = fetch(f"{repository}/{arch}/{package['P']}-{package['V']}.apk")
        members = gzip_members(data)
        if len(members) != 3:
            sys.exit(f"{package['P']} is not a package with a signature, a control part, and data")
        control = "Q1" + base64.b64encode(hashlib.sha1(members[1][0]).digest()).decode()
        if control != package["C"]:
            sys.exit(f"the checksum of {package['P']} does not match the index")
        info = tar_files(members[1][1])[".PKGINFO"].decode()
        datahash = next(
            (line.split("=", 1)[1].strip() for line in info.splitlines() if line.startswith("datahash")),
            None,
        )
        if datahash != hashlib.sha256(members[2][0]).hexdigest():
            sys.exit(f"the data of {package['P']} does not match its control part")
        with tarfile.open(fileobj=io.BytesIO(members[2][1]), mode="r:") as archive:
            for member in archive:
                if member.name.startswith(".SIGN.") or member.name.startswith(".PKGINFO"):
                    continue
                member.pax_headers = {}
                member.mtime = now
                content = archive.extractfile(member).read() if member.isfile() else None
                entries.append((member, content))
        print(f"added {package['P']} {package['V']}", file=sys.stderr)
    return entries


def entry(name: str, kind: bytes, mode: int, now: int, data: Optional[bytes] = None, link: str = "") -> Entry:
    info = tarfile.TarInfo(name)
    info.type = kind
    info.mode = mode
    info.mtime = now
    info.linkname = link
    if data is not None:
        info.size = len(data)
    return info, data


def extra_entries(component: str, bin_dir: str, now: int) -> list[Entry]:
    spec = COMPONENTS[component]
    name = spec["name"]
    data = f"/var/lib/{name}"
    lib = f"/usr/local/lib/{name}"
    ssh = bool(spec["packages"])

    def binary(binary_name: str) -> bytes:
        with open(os.path.join(bin_dir, binary_name), "rb") as f:
            return f.read()

    def script(path: str, text: str) -> Entry:
        return entry(f"{lib.lstrip('/')}/{path}", tarfile.REGTYPE, 0o755, now, text.encode())

    entries = [
        entry("etc/inittab", tarfile.REGTYPE, 0o644, now, inittab(lib, ssh).encode()),
        entry("etc/hosts", tarfile.REGTYPE, 0o644, now, HOSTS.encode()),
        entry("etc/hostname", tarfile.REGTYPE, 0o644, now, f"{name}\n".encode()),
    ]
    entries += [
        entry(f"usr/local/bin/{binary_name}", tarfile.REGTYPE, 0o755, now, binary(binary_name))
        for binary_name in spec["binaries"]
    ]
    entries += [
        entry(lib.lstrip("/"), tarfile.DIRTYPE, 0o755, now),
        script("init", init_script()),
        script("run", run_script(name, data)),
        script("rotate", rotate_script(data)),
    ]
    if ssh:
        entries.append(script("sshd", sshd_script(name, data)))
    entries += [
        entry(data.lstrip("/"), tarfile.DIRTYPE, 0o755, now),
        entry("root/.config", tarfile.SYMTYPE, 0o777, now, link=f"{data}/config"),
        entry("root/.local", tarfile.DIRTYPE, 0o755, now),
        entry("root/.local/share", tarfile.SYMTYPE, 0o777, now, link=f"{data}/data"),
        entry("root/.local/state", tarfile.SYMTYPE, 0o777, now, link=f"{data}/state"),
    ]
    return entries


def write_rootfs(path: str, base: bytes, extras: list[Entry]) -> None:
    with tarfile.open(fileobj=io.BytesIO(base), mode="r:*") as source, tarfile.open(path, "w:xz", format=tarfile.PAX_FORMAT) as out:
        members = source.getmembers()
        existing = {member.name.removeprefix("./").rstrip("/") for member in members}
        extras = [(info, data) for info, data in extras if not (info.isdir() and info.name.rstrip("/") in existing)]
        replaced = {info.name.rstrip("/") for info, _ in extras}
        for member in members:
            name = member.name.removeprefix("./").rstrip("/")
            if name in replaced:
                continue
            data = source.extractfile(member).read() if member.isfile() else None
            out.addfile(member, io.BytesIO(data) if data is not None else None)
        for info, data in extras:
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            out.addfile(info, io.BytesIO(data) if data is not None else None)


def write_metadata(path: str, component: str, arch: str, version: str, alpine: str, now: int) -> None:
    name = COMPONENTS[component]["name"]
    text = (
        f"architecture: {ARCHES[arch]}\n"
        f"creation_date: {now}\n"
        "properties:\n"
        f"  description: {name} {version} (Alpine Linux {alpine})\n"
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
        description="Build the Incus image of loom-host or loom-oracle as a metadata tarball and a rootfs tarball."
    )
    parser.add_argument("--component", choices=sorted(COMPONENTS), default="host")
    parser.add_argument("--arch", choices=sorted(ARCHES), required=True)
    parser.add_argument("--bin-dir", required=True, help="the directory with the binaries of the component for this architecture")
    parser.add_argument("--output", required=True, help="the directory of the two tarballs")
    parser.add_argument("--version", default="dev")
    parser.add_argument("--name", help="the file name of the tarballs without the suffix; the default is <component>-<arch>")
    parser.add_argument("--base", help="an Alpine minirootfs tarball; the default downloads the latest stable release")
    args = parser.parse_args()

    if args.base:
        with open(args.base, "rb") as f:
            base, alpine = f.read(), "custom"
    else:
        base, alpine = alpine_rootfs(ARCHES[args.arch])
    now = int(time.time())
    extras = alpine_packages(base, COMPONENTS[args.component]["packages"], now)
    extras += extra_entries(args.component, args.bin_dir, now)
    os.makedirs(args.output, exist_ok=True)
    name = args.name or f"{COMPONENTS[args.component]['name']}-{args.arch}"
    prefix = os.path.join(args.output, name)
    write_rootfs(f"{prefix}.rootfs.tar.xz", base, extras)
    write_metadata(f"{prefix}.metadata.tar.xz", args.component, args.arch, args.version, alpine, now)
    print(f"{prefix}.metadata.tar.xz")
    print(f"{prefix}.rootfs.tar.xz")


if __name__ == "__main__":
    main()
