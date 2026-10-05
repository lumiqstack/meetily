#!/usr/bin/env python3
"""Redact a captured SharePoint REST response before committing it.

Usage: python3 redact.py <captured.json> <output.json>

Replaces tenant hosts with contoso, OneDrive personal paths with a fixed
user, e-mail addresses, and GUIDs. Meeting titles are kept by default because
the import logic depends on their exact shape; pass --titles to replace the
text before the Teams "-YYYYMMDD_HHMMSS-" stamp as well. Always read the
output before committing it.
"""
import json
import re
import sys

def redact(text: str, titles: bool) -> str:
    text = re.sub(r"[A-Za-z0-9-]+-my\.sharepoint\.com", "contoso-my.sharepoint.com", text)
    text = re.sub(r"(?<![A-Za-z0-9-])[A-Za-z0-9-]+\.sharepoint\.com", "contoso.sharepoint.com", text)
    text = re.sub(r"/personal/[A-Za-z0-9_]+", "/personal/jdoe_contoso_com", text)
    text = re.sub(r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}", "jdoe@contoso.com", text)
    text = re.sub(r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
                  "00000000-0000-0000-0000-000000000000", text)
    if titles:
        counter = iter(range(1, 100000))
        text = re.sub(r"([/\"])([^/\"]+?)(-\d{8}_\d{6}-Meeting (?:Recording|Transcript))",
                      lambda m: f"{m.group(1)}Meeting {next(counter)}{m.group(3)}", text)
    return text

def main() -> None:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 2:
        sys.exit(__doc__)
    raw = open(args[0], encoding="utf-8").read()
    json.loads(raw)  # fail early on a truncated capture
    out = redact(raw, "--titles" in sys.argv)
    json.loads(out)
    open(args[1], "w", encoding="utf-8").write(out)
    print(f"wrote {args[1]} — review it before committing")

if __name__ == "__main__":
    main()
