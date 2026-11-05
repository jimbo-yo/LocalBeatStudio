#!/usr/bin/env bash
# flac2phone.sh - convert a FLAC library to Opus or AAC for a phone,
# keeping the album folder structure, tags, and cover art.
#
# Usage:
#   flac2phone.sh SOURCE_DIR DEST_DIR [opus|aac] [bitrate_kbps]
#
# Examples:
#   flac2phone.sh ~/Music/FLAC ~/Music/Phone              # Opus, 160 kbps
#   flac2phone.sh ~/Music/FLAC ~/Music/Phone opus 128     # Opus, 128 kbps
#   flac2phone.sh ~/Music/FLAC ~/Music/Phone-AAC aac      # AAC, 256 kbps
#
# Re-running it only converts new or changed FLAC files, so you can run it
# again after ripping more CDs.
#
# Needs: opus-tools (for Opus) or ffmpeg (for AAC)
#   sudo dnf install opus-tools ffmpeg-free

set -euo pipefail

usage() {
    echo "Usage: $(basename "$0") SOURCE_DIR DEST_DIR [opus|aac] [bitrate_kbps]" >&2
    exit 1
}

[[ $# -lt 2 ]] && usage
[[ -d $1 ]] || { echo "Source folder not found: $1" >&2; exit 1; }

SRC=$(realpath "$1")
DEST=$(realpath -m "$2")
FORMAT=${3:-opus}

case $FORMAT in
    opus) BITRATE=${4:-160}; EXT=opus
          command -v opusenc >/dev/null || { echo "opusenc not found: sudo dnf install opus-tools" >&2; exit 1; } ;;
    aac)  BITRATE=${4:-256}; EXT=m4a
          command -v ffmpeg >/dev/null || { echo "ffmpeg not found: sudo dnf install ffmpeg-free" >&2; exit 1; } ;;
    *)    usage ;;
esac

export SRC DEST FORMAT BITRATE EXT

convert_one() {
    local in=$1
    local rel=${in#"$SRC"/}
    local out="$DEST/${rel%.*}.$EXT"
    local tmp="$out.part"

    # Skip files already converted and newer than the source
    if [[ -f $out && $out -nt $in ]]; then
        return 0
    fi

    mkdir -p "$(dirname "$out")"

    if [[ $FORMAT == opus ]]; then
        # opusenc reads FLAC directly and copies tags and embedded cover art
        opusenc --quiet --bitrate "$BITRATE" "$in" "$tmp"
    else
        local art=()
        # Carry over embedded cover art if the FLAC has any
        if ffprobe -v error -select_streams v -show_entries stream=index -of csv=p=0 "$in" | grep -q .; then
            art=(-map 0:v:0 -c:v copy -disposition:v:0 attached_pic)
        fi
        ffmpeg -nostdin -loglevel error -y -i "$in" \
            -map 0:a:0 -c:a aac -b:a "${BITRATE}k" \
            "${art[@]}" -map_metadata 0 \
            -f ipod "$tmp"
    fi

    if [[ $? -eq 0 && -s $tmp ]]; then
        mv "$tmp" "$out"
        echo "OK    $rel"
    else
        rm -f "$tmp"
        echo "FAIL  $rel" >&2
    fi
}
export -f convert_one

echo "Converting FLAC in: $SRC"
echo "Writing $FORMAT ($BITRATE kbps) to: $DEST"
echo

# Convert using all CPU cores
# (skips the destination folder, in case it is inside the source)
find "$SRC" -path "$DEST" -prune -o -type f -iname '*.flac' -print0 |
    xargs -0 -P "$(nproc)" -I{} bash -c 'convert_one "$1" || true' _ {}

# Copy folder cover images (cover.jpg, folder.jpg, etc.) so players that
# look for them find them
find "$SRC" -path "$DEST" -prune -o -type f \( -iname 'cover.jpg' -o -iname 'cover.png' \
                    -o -iname 'folder.jpg' -o -iname 'folder.png' \) -print0 |
    while IFS= read -r -d '' img; do
        dst="$DEST/${img#"$SRC"/}"
        mkdir -p "$(dirname "$dst")"
        cp -u "$img" "$dst"
    done

echo
echo "Done."
