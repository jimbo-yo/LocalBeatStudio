#!/usr/bin/env python3
"""fetch-cover.py - find and embed front cover art for albums that have none.

For each album folder of FLACs (e.g. old fre:ac rips with no MusicBrainz IDs):
  1. look the album up on MusicBrainz by its ARTIST/ALBUM tags (bracketed junk
     like "(Virgin US-291610)" or "(sdtk)" is dropped from the title), or use
     --release-group MBID
  2. download the front cover from the Cover Art Archive (1200 px)
  3. save it as cover.jpg in the folder and embed it (front cover, type 3) in
     every FLAC that has no front cover yet

Usage:
  fetch-cover.py [--preview DIR] [--release-group MBID] [--artist A] [--title T] ALBUM_DIR...

--preview DIR: only download each cover to DIR/<folder>.jpg and print the
match, so the covers can be checked first. Nothing in the album is changed.

Embedding changes the FLACs' modified time, so flac2phone.sh re-makes their
Opus copies. Needs: metaflac. Network: musicbrainz.org, coverartarchive.org.
"""

import argparse
import json
import re
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

USER_AGENT = "CDRipPipeline-fetch-cover/1.0 (personal CD ripping scripts)"
NO_ARTIST = {"soundtrack", "various artists", "various", "va"}
_last = 0.0


def http(url, as_json=True):
    global _last
    for attempt in range(5):
        wait = 1.1 - (time.monotonic() - _last)
        if wait > 0:
            time.sleep(wait)
        _last = time.monotonic()
        req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
        try:
            with urllib.request.urlopen(req, timeout=60) as r:
                data = r.read()
                return json.loads(data) if as_json else data
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            if e.code in (429, 500, 502, 503) and attempt < 4:
                time.sleep(2 + 2 * attempt)
                continue
            raise
        except urllib.error.URLError:
            if attempt < 4:
                time.sleep(3)
                continue
            raise


def first_tag(flac, *keys):
    for k in keys:
        out = subprocess.run(["metaflac", f"--show-tag={k}", str(flac)], capture_output=True, text=True).stdout
        for line in out.splitlines():
            if "=" in line and line.split("=", 1)[1].strip():
                return line.split("=", 1)[1].strip()
    return ""


def has_front_cover(flac):
    out = subprocess.run(["metaflac", "--list", "--block-type=PICTURE", str(flac)], capture_output=True, text=True).stdout
    return re.search(r"^\s*type: 3\b", out, re.M) is not None


def clean_title(title):
    return re.sub(r"\s*[\(\[][^)\]]*[\)\]]", "", title).strip() or title


def lucene(s):
    return '"' + re.sub(r'([\\"])', r"\\\1", s) + '"'


def find_release_groups(artist, title):
    """Best matches first: albums before singles/EPs, then MusicBrainz's score."""
    soundtrack = artist.lower() in NO_ARTIST
    titles = [title] + ([title.replace(" - ", ": ")] if " - " in title else [])
    for t in titles:
        q = "releasegroup:" + lucene(t)
        if soundtrack:
            q += " AND secondarytype:soundtrack"
        else:
            # straight and curly apostrophes are spelled differently on MusicBrainz
            q += " AND artist:" + lucene(artist.replace("’", "'"))
        data = http("https://musicbrainz.org/ws/2/release-group?fmt=json&limit=15&query=" + urllib.parse.quote(q))
        groups = [g for g in (data or {}).get("release-groups", []) if g.get("score", 0) >= 70]
        if groups:
            return sorted(groups, key=lambda g: (g.get("primary-type") != "Album", -g.get("score", 0)))
    return []


def describe(rg):
    artist = "".join(c["name"] + c.get("joinphrase", "") for c in rg.get("artist-credit", []))
    return f"{artist} – {rg['title']} ({rg.get('first-release-date', '?')[:4]}, {rg.get('primary-type', '?')}) [{rg['id']}]"


def main():
    ap = argparse.ArgumentParser(description="Find and embed front cover art for album folders without one.")
    ap.add_argument("albums", nargs="+", type=Path)
    ap.add_argument("--preview", type=Path, help="only download covers to this folder for checking")
    ap.add_argument("--release-group", help="use this MusicBrainz release group (one album only)")
    ap.add_argument("--artist", help="search with this artist instead of the tags")
    ap.add_argument("--title", help="search with this album title instead of the tags")
    args = ap.parse_args()
    if args.release_group and len(args.albums) > 1:
        ap.error("--release-group works on one album at a time")

    failed = 0
    for album in args.albums:
        flacs = sorted(album.glob("*.flac"))
        if not flacs:
            print(f"SKIP  {album}: no FLAC files")
            continue
        artist = args.artist or first_tag(flacs[0], "ALBUMARTIST", "ARTIST")
        title = args.title or clean_title(first_tag(flacs[0], "ALBUM"))

        if args.release_group:
            candidates = [{"id": args.release_group, "title": title, "artist-credit": []}]
        else:
            candidates = find_release_groups(artist, title)
        cover, chosen = None, None
        for rg in candidates[:5]:
            cover = http(f"https://coverartarchive.org/release-group/{rg['id']}/front-1200", as_json=False)
            if cover:
                chosen = rg
                break
        if not cover:
            print(f"FAIL  {album.name}: no cover found (searched {artist!r} / {title!r})", file=sys.stderr)
            failed += 1
            continue

        if args.preview:
            args.preview.mkdir(parents=True, exist_ok=True)
            (args.preview / f"{album.name}.jpg").write_bytes(cover)
            print(f"FOUND {album.name}: {describe(chosen)}")
            continue

        jpg = album / "cover.jpg"
        jpg.write_bytes(cover)
        todo = [f for f in flacs if not has_front_cover(f)]
        for f in todo:
            subprocess.run(["metaflac", f"--import-picture-from=3|image/jpeg|Front cover||{jpg}", str(f)], check=True)
        print(f"COVER {album.name}: {describe(chosen)}; embedded in {len(todo)} of {len(flacs)} files")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
