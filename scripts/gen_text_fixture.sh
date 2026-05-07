#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/gen_text_fixture.sh [--seed N] [--line-min N] [--line-max N] SIZE OUTPUT

Generate random sentence-like text fixture with exact target size.

Examples:
  scripts/gen_text_fixture.sh 512kb test_assets/sample.txt
  scripts/gen_text_fixture.sh --seed 7 --line-min 20 --line-max 60 1.5mb /tmp/big.txt
EOF
}

parse_size() {
    local size_text="$1"

    awk -v size_text="$size_text" '
    BEGIN {
        normalized = tolower(size_text)
        gsub(/^[[:space:]]+|[[:space:]]+$/, "", normalized)

        if (match(normalized, /^([0-9]+([.][0-9]+)?) *([kmg]b?)$/, parts) == 0) {
            print "invalid size '\''" size_text "'\''; use formats like 512kb, 20mb, or 1.5gb" > "/dev/stderr"
            exit 1
        }

        amount = parts[1] + 0
        if (amount <= 0) {
            print "size must be greater than zero" > "/dev/stderr"
            exit 1
        }

        unit = parts[3]
        multiplier = unit ~ /^k/ ? 1024 : unit ~ /^m/ ? 1024 * 1024 : 1024 * 1024 * 1024
        size_bytes = int(amount * multiplier)
        if (size_bytes < 1) {
            size_bytes = 1
        }

        print size_bytes
    }'
}

line_min=40
line_max=120
seed=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --seed)
            seed="${2:?missing value for --seed}"
            shift 2
            ;;
        --line-min)
            line_min="${2:?missing value for --line-min}"
            shift 2
            ;;
        --line-max)
            line_max="${2:?missing value for --line-max}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        -*)
            printf 'unknown option: %s\n' "$1" >&2
            usage >&2
            exit 1
            ;;
        *)
            break
            ;;
    esac
done

if [[ $# -ne 2 ]]; then
    usage >&2
    exit 1
fi

if ! [[ "$line_min" =~ ^[0-9]+$ ]] || (( line_min < 8 )); then
    printf '%s\n' "--line-min must be integer >= 8" >&2
    exit 1
fi

if ! [[ "$line_max" =~ ^[0-9]+$ ]] || (( line_max < line_min )); then
    printf '%s\n' "--line-max must be integer >= --line-min" >&2
    exit 1
fi

size_text="$1"
output_path="$2"
size_bytes="$(parse_size "$size_text")"

mkdir -p "$(dirname "$output_path")"

awk \
    -v target="$size_bytes" \
    -v min_cols="$line_min" \
    -v max_cols="$line_max" \
    -v seed_value="$seed" '
BEGIN {
    if (seed_value == "") {
        srand()
    } else {
        srand(seed_value)
    }

    bytes_written = 0
    while (bytes_written < target) {
        remaining = target - bytes_written
        line = random_sentence(min_cols, max_cols)
        if (length(line) > remaining) {
            line = substr(line, 1, remaining)
        }
        printf "%s", line
        bytes_written += length(line)
    }
}

function random_int(min_value, max_value) {
    return int(rand() * (max_value - min_value + 1)) + min_value
}

function random_word(    length_value, word, i) {
    length_value = random_int(2, 12)
    word = ""
    for (i = 0; i < length_value; i++) {
        word = word sprintf("%c", random_int(97, 122))
    }
    return word
}

function capitalize_ascii(text) {
    return toupper(substr(text, 1, 1)) substr(text, 2)
}

function punctuation() {
    value = random_int(1, 5)
    if (value <= 3) {
        return "."
    }
    if (value == 4) {
        return "!"
    }
    return "?"
}

function random_sentence(min_value, max_value,    target_width, sentence, word, candidate) {
    target_width = random_int(min_value, max_value)
    sentence = ""

    while (length(sentence) < target_width || split(sentence, _parts, " ") < 3) {
        word = random_word()
        candidate = sentence == "" ? word : sentence " " word
        if (sentence != "" && length(candidate) > max_value) {
            break
        }
        sentence = candidate
    }

    if (sentence == "") {
        sentence = random_word()
    }

    return capitalize_ascii(sentence) punctuation() "\n"
}
' > "$output_path"

printf 'wrote %s bytes to %s\n' "$size_bytes" "$output_path"
