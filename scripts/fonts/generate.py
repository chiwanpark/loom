#!/usr/bin/env python3
"""Generate the Android app's catalog of Google Fonts.

The app lets the user search Google Fonts for the font of its text and of
code, and Google Play services downloads the fonts it picks. This script
writes the families it can pick from to
android/app/src/main/assets/google-fonts.tsv, one family per line, the most
popular first:

    name<TAB>category<TAB>weights<TAB>flags<TAB>scripts

- category: sans-serif, serif, display, handwriting, or monospace;
- weights: the upright weights of the family, such as 400,500,700;
- flags: "m" when the family is monospaced: in the monospace category, or
  with Mono, Code, or Coding in its name, as the categories put some
  monospaced families elsewhere (Nanum Gothic Coding is "handwriting", Noto
  Sans Mono "sans-serif");
- scripts: the scripts other than Latin that the family covers, such as
  korean,japanese, with the Chinese subsets as chinese and the extended
  subsets under their script.

It lists the families that both of these name, so each one can be
downloaded and each one has a category:

- https://fonts.gstatic.com/s/a/directory.xml, the directory of the Google
  Play services font provider, which Android Studio reads too: the families
  the provider serves and their weights;
- https://fonts.google.com/metadata/fonts, the metadata behind
  fonts.google.com: each family's category, scripts, and popularity.

Usage: scripts/fonts/generate.py  (needs network access)
"""

import json
import re
import urllib.request
import xml.etree.ElementTree as ElementTree
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "android/app/src/main/assets/google-fonts.tsv"
DIRECTORY = "https://fonts.gstatic.com/s/a/directory.xml"
METADATA = "https://fonts.google.com/metadata/fonts"

CATEGORIES = {
    "Sans Serif": "sans-serif",
    "Serif": "serif",
    "Display": "display",
    "Handwriting": "handwriting",
    "Monospace": "monospace",
}
MONOSPACED_NAME = re.compile(r"\b(Mono|Code|Coding)\b")
# Subsets that are not a script of their own.
NOT_SCRIPTS = {"menu", "latin", "vietnamese", "math", "symbols", "symbols2", "emoji", "braille"}


def fetch(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "loom-font-catalog"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return response.read()


def provider_weights() -> dict[str, list[int]]:
    """The upright, normal-width weights of every family the provider serves."""
    root = ElementTree.fromstring(fetch(DIRECTORY))
    families = {}
    for family in root.iter("family"):
        weights = sorted(
            {
                int(font.get("weight"))
                for font in family.iter("font")
                if float(font.get("italic", "0")) == 0 and float(font.get("width", "100")) == 100
            }
        )
        if weights:
            families[family.get("name")] = weights
    return families


def scripts(subsets: list[str]) -> list[str]:
    """The scripts of a family's subsets, in the order the metadata lists them."""
    found = []
    for subset in subsets:
        script = subset.removesuffix("-ext")
        if script.startswith("chinese-"):
            script = "chinese"
        if script in NOT_SCRIPTS or "," in script or script in found:
            continue
        found.append(script)
    return found


def main() -> None:
    weights = provider_weights()
    metadata = json.loads(fetch(METADATA))["familyMetadataList"]
    rows = []
    for family in metadata:
        name = family["family"]
        category = CATEGORIES.get(family["category"])
        if name not in weights or category is None or "\t" in name:
            continue
        flags = "m" if category == "monospace" or MONOSPACED_NAME.search(name) else ""
        found = ",".join(scripts(family["subsets"]))
        rows.append((family.get("popularity") or 1_000_000, name, category, weights[name], flags, found))
    rows.sort(key=lambda row: (row[0], row[1]))
    lines = [
        f"{name}\t{category}\t{','.join(map(str, ws))}\t{flags}\t{found}"
        for _, name, category, ws, flags, found in rows
    ]
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"wrote {len(lines)} families to {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
