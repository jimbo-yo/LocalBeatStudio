#!/usr/bin/env python3
"""mb-enrich.py - add the tags cyanrip doesn't write, from MusicBrainz.

For every FLAC with a MusicBrainz release ID (MUSICBRAINZ_ALBUMID) it adds:

  ORIGINALDATE / ORIGINALYEAR  the album's first release (the release group's
                               first-release-date), not this CD's DATE
  GENRE                        up to 3 genres, most-voted first: the album's
                               (release group) genres, else the artist's

Usage:
  mb-enrich.py [--force] [--dry-run] [PATH ...]

PATH is album folders or a whole library (default: $FLAC_DIR or ~/Music/FLAC).
Files that already have ORIGINALDATE are skipped unless --force. Files without
a MusicBrainz ID (old fre:ac rips) are skipped.

Changing the tags updates the FLAC's modified time, so the next flac2phone.sh
run re-makes its Opus copy, and Phone Sync then replaces it on the phone.

Needs: metaflac (flac). Network: musicbrainz.org only, at most 1 request/s.
"""

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from collections import defaultdict
from pathlib import Path

API = "https://musicbrainz.org/ws/2/"
USER_AGENT = "CDRipPipeline-mb-enrich/1.0 (personal CD ripping scripts)"
MAX_GENRES = 3
SMALL_WORDS = {"a", "an", "and", "of", "the", "n", "in", "on", "or", "for", "to"}

_last_request = 0.0


def mb_get(path):
    """GET a MusicBrainz JSON resource, keeping to 1 request/s and retrying 503s."""
    global _last_request
    for attempt in range(5):
        wait = 1.1 - (time.monotonic() - _last_request)
        if wait > 0:
            time.sleep(wait)
        _last_request = time.monotonic()
        sep = "&" if "?" in path else "?"
        req = urllib.request.Request(API + path + sep + "fmt=json", headers={"User-Agent": USER_AGENT})
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                return json.load(resp)
        except urllib.error.HTTPError as e:
            if e.code in (503, 429) and attempt < 4:
                time.sleep(2 + attempt * 2)  # rate limited: back off
                continue
            raise
        except urllib.error.URLError:
            if attempt < 4:
                time.sleep(3)
                continue
            raise


def title_case(genre):
    """'rock and roll' -> 'Rock and Roll', 'proto-punk' -> 'Proto-Punk'."""
    words = genre.split(" ")
    out = []
    for i, w in enumerate(words):
        if i > 0 and w in SMALL_WORDS:
            out.append(w)
        else:
            out.append("-".join(p[:1].upper() + p[1:] for p in w.split("-")))
    return " ".join(out)


def top_genres(entity):
    genres = sorted(entity.get("genres", []), key=lambda g: (-g.get("count", 0), g["name"]))
    return [title_case(g["name"]) for g in genres if g.get("count", 0) > 0][:MAX_GENRES]


def album_info(release_id):
    """(original date, [genres]) for a release."""
    release = mb_get(f"release/{release_id}?inc=release-groups+artist-credits")
    rg = release["release-group"]
    date = rg.get("first-release-date") or release.get("date") or ""
    genres = top_genres(mb_get(f"release-group/{rg['id']}?inc=genres"))
    if not genres:
        credits = release.get("artist-credit") or []
        if credits:
            genres = top_genres(mb_get(f"artist/{credits[0]['artist']['id']}?inc=genres"))
    return date, genres


def tags(flac):
    """The FLAC's Vorbis comments as {UPPERCASE KEY: [values]}."""
    out = subprocess.run(["metaflac", "--export-tags-to=-", str(flac)], capture_output=True, text=True, check=True).stdout
    result = defaultdict(list)
    for line in out.splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            result[k.upper()].append(v)
    return result


def write(flac, date, genres):
    args = ["metaflac", "--remove-tag=ORIGINALDATE", "--remove-tag=ORIGINALYEAR"]
    if date:
        args += [f"--set-tag=ORIGINALDATE={date}", f"--set-tag=ORIGINALYEAR={date[:4]}"]
    if genres:
        args += ["--remove-tag=GENRE"] + [f"--set-tag=GENRE={g}" for g in genres]
    subprocess.run(args + [str(flac)], check=True)


def main():
    ap = argparse.ArgumentParser(description="Add original release date and genres from MusicBrainz to FLACs.")
    ap.add_argument("paths", nargs="*", help="album folders or a library (default: $FLAC_DIR or ~/Music/FLAC)")
    ap.add_argument("--force", action="store_true", help="redo files that already have ORIGINALDATE")
    ap.add_argument("--dry-run", "-n", action="store_true", help="show what would change; write nothing")
    args = ap.parse_args()

    paths = [Path(p) for p in args.paths] or [Path(os.environ.get("FLAC_DIR", Path.home() / "Music/FLAC"))]
    flacs = sorted(f for p in paths for f in ([p] if p.is_file() else p.rglob("*.flac")) if f.suffix.lower() == ".flac")
    if not flacs:
        print("No FLAC files found.")
        return 0

    # Group by release so each album costs one set of lookups.
    by_release, skipped_no_id, skipped_done = defaultdict(list), 0, 0
    for f in flacs:
        t = tags(f)
        mbid = (t.get("MUSICBRAINZ_ALBUMID") or [""])[0].strip()
        if not mbid:
            skipped_no_id += 1
        elif t.get("ORIGINALDATE") and not args.force:
            skipped_done += 1
        else:
            by_release[mbid].append(f)

    failed = 0
    for mbid, files in by_release.items():
        album = files[0].parent
        try:
            date, genres = album_info(mbid)
        except Exception as e:  # network trouble: leave these for next time
            print(f"FAIL  {album}: {e}", file=sys.stderr)
            failed += len(files)
            continue
        print(f"{'WOULD' if args.dry_run else 'TAG  '} {album.name}: {date or '(no date)'} · {'; '.join(genres) or '(no genres)'} ({len(files)} files)")
        if not args.dry_run:
            for f in files:
                write(f, date, genres)

    notes = []
    if skipped_done:
        notes.append(f"{skipped_done} already done")
    if skipped_no_id:
        notes.append(f"{skipped_no_id} without a MusicBrainz ID")
    if notes:
        print("Skipped " + ", ".join(notes) + ".")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
