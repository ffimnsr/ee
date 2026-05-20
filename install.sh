#!/bin/sh
# shellcheck shell=dash
# shellcheck disable=SC3043

# Official ee installer.
# Runs on Unix-like shells and installs latest ee release from GitHub.

REPO_OWNER="ffimnsr"
REPO_NAME="ee"
PACKAGE_NAME="ee"

main() {
    if [ "${KSH_VERSION-}" = 'Version JM 93t+ 2010-03-05' ]; then
        err 'installer does not work with this ksh93 version; please try bash'
    fi

    set -u
    parse_args "$@"

    local _arch
    _arch="${ARCH:-$(ensure get_architecture)}"
    assert_nz "${_arch}" "arch"
    echo "Detected architecture: ${_arch}"

    local _bin_name
    _bin_name="${PACKAGE_NAME}"

    local _tmp_dir
    _tmp_dir="$(mktemp -d)" || err "mktemp: could not create temporary directory"
    cd "${_tmp_dir}" || err "cd: failed to enter directory: ${_tmp_dir}"
    echo "Temporary directory: ${_tmp_dir}"

    local _package
    _package="$(ensure download_ee "${_arch}")"
    assert_nz "${_package}" "package"
    echo "Downloaded package: ${_package}"

    maybe_verify_checksum "${_package}"

    case "${_package}" in
        *.tar.gz) need_cmd tar; ensure tar -xf "${_package}" ;;
        *.zip) need_cmd unzip; ensure unzip -oq "${_package}" ;;
        *) err "unsupported package format: ${_package}" ;;
    esac

    local _filename_no_ext
    _filename_no_ext=$(basename "${_package}" | sed -E 's/\.(tar\.gz|zip)$//')

    ensure try_sudo mkdir -p -- "${BIN_DIR}"
    ensure try_sudo cp -- "${_filename_no_ext}/${_bin_name}" "${BIN_DIR}/${_bin_name}"
    ensure try_sudo chmod +x "${BIN_DIR}/${_bin_name}"
    echo "Installed ${PACKAGE_NAME} to ${BIN_DIR}"

    local _bundle_src
    _bundle_src=""
    if [ -d "${_filename_no_ext}/runtime" ]; then
        _bundle_src="${_filename_no_ext}/runtime"
    elif [ -d "${_filename_no_ext}/share/${PACKAGE_NAME}" ]; then
        _bundle_src="${_filename_no_ext}/share/${PACKAGE_NAME}"
    fi
    if [ -n "${_bundle_src}" ]; then
        ensure try_sudo mkdir -p -- "${RUNTIME_DIR}"
        ensure try_sudo cp -R -- "${_bundle_src}/." "${RUNTIME_DIR}/"
        echo "Installed bundled runtime and plugins to ${RUNTIME_DIR}"
    fi

    if [ -f "${_filename_no_ext}/README.md" ]; then
        ensure try_sudo mkdir -p -- "${DOC_DIR}/${PACKAGE_NAME}"
        ensure try_sudo cp -- "${_filename_no_ext}/README.md" "${DOC_DIR}/${PACKAGE_NAME}/README.md"
        echo "Installed documentation to ${DOC_DIR}"
    fi

    ensure try_sudo mkdir -p -- "${LIC_DIR}/${PACKAGE_NAME}"
    for _file in LICENSE LICENSE-APACHE; do
        if [ -f "${_filename_no_ext}/${_file}" ]; then
            ensure try_sudo cp -- "${_filename_no_ext}/${_file}" "${LIC_DIR}/${PACKAGE_NAME}/${_file}"
        fi
    done
    echo "Installed license files to ${LIC_DIR}"

    echo ""
    echo "${PACKAGE_NAME} is installed!"
    if ! echo ":${PATH}:" | grep -Fq ":${BIN_DIR}:"; then
        echo "Note: ${BIN_DIR} is not on your \$PATH."
        echo "Add it to your shell profile before using ${PACKAGE_NAME}."
    fi

    maybe_install_completions "${BIN_DIR}"
}

maybe_install_completions() {
    local _bin_dir _shell _rc_file _eval_line _comp_dir _comp_file
    _bin_dir="$1"

    # Skip when stdin is not a terminal (non-interactive installs).
    if ! [ -t 0 ]; then
        return 0
    fi

    printf "Install shell completions? [y/N] "
    read -r _answer </dev/tty
    case "${_answer}" in
        [yY]|[yY][eE][sS]) ;;
        *) return 0 ;;
    esac

    # Detect shell from $SHELL; fall back to asking the user.
    _shell="$(basename "${SHELL:-}")"
    case "${_shell}" in
        bash|zsh|fish) ;;
        *)
            printf "Shell not detected. Enter shell name (bash/zsh/fish): "
            read -r _shell </dev/tty
            ;;
    esac

    _ee="${_bin_dir}/${PACKAGE_NAME}"

    case "${_shell}" in
        bash)
            _rc_file="${HOME}/.bashrc"
            _eval_line='eval "$(ee completions bash)"'
            if ! grep -qF 'ee completions' "${_rc_file}" 2>/dev/null; then
                printf '\n%s\n' "${_eval_line}" >> "${_rc_file}"
                echo "Added completions eval to ${_rc_file}"
            else
                echo "Completions already configured in ${_rc_file}"
            fi
            ;;
        zsh)
            _rc_file="${HOME}/.zshrc"
            _eval_line='eval "$(ee completions zsh)"'
            if ! grep -qF 'ee completions' "${_rc_file}" 2>/dev/null; then
                printf '\n%s\n' "${_eval_line}" >> "${_rc_file}"
                echo "Added completions eval to ${_rc_file}"
            else
                echo "Completions already configured in ${_rc_file}"
            fi
            ;;
        fish)
            _comp_dir="${HOME}/.config/fish/completions"
            _comp_file="${_comp_dir}/ee.fish"
            mkdir -p "${_comp_dir}"
            "${_ee}" completions fish > "${_comp_file}" 2>/dev/null \
                || err "failed to generate fish completions"
            echo "Installed fish completions to ${_comp_file}"
            ;;
        *)
            echo "Unsupported shell '${_shell}'. Run 'ee completions <shell>' manually."
            return 0
            ;;
    esac

    echo "Restart your shell or source the rc file to enable completions."
}

parse_args() {
    BIN_DIR_DEFAULT="${HOME}/.local/bin"
    DATA_HOME_DEFAULT="${XDG_DATA_HOME:-${HOME}/.local/share}"
    DOC_DIR_DEFAULT="${DATA_HOME_DEFAULT}/doc"
    LIC_DIR_DEFAULT="${DATA_HOME_DEFAULT}/licenses"
    RUNTIME_DIR_DEFAULT="${DATA_HOME_DEFAULT}/${PACKAGE_NAME}"
    SUDO_DEFAULT="sudo"
    BIN_DIR="${BIN_DIR_DEFAULT}"
    DOC_DIR="${DOC_DIR_DEFAULT}"
    LIC_DIR="${LIC_DIR_DEFAULT}"
    RUNTIME_DIR="${RUNTIME_DIR_DEFAULT}"
    SUDO="${SUDO_DEFAULT}"

    while [ "$#" -gt 0 ]; do
        case "$1" in
            --arch) ARCH="$2" && shift 2 ;;
            --arch=*) ARCH="${1#*=}" && shift 1 ;;
            --bin-dir) BIN_DIR="$2" && shift 2 ;;
            --bin-dir=*) BIN_DIR="${1#*=}" && shift 1 ;;
            --doc-dir) DOC_DIR="$2" && shift 2 ;;
            --doc-dir=*) DOC_DIR="${1#*=}" && shift 1 ;;
            --license-dir) LIC_DIR="$2" && shift 2 ;;
            --license-dir=*) LIC_DIR="${1#*=}" && shift 1 ;;
            --runtime-dir) RUNTIME_DIR="$2" && shift 2 ;;
            --runtime-dir=*) RUNTIME_DIR="${1#*=}" && shift 1 ;;
            --sudo) SUDO="$2" && shift 2 ;;
            --sudo=*) SUDO="${1#*=}" && shift 1 ;;
            -h|--help) usage && exit 0 ;;
            *) err "Unknown option: $1" ;;
        esac
    done
}

usage() {
    local _text_heading _text_reset _arch
    _text_heading="$(tput bold || true 2>/dev/null)$(tput smul || true 2>/dev/null)"
    _text_reset="$(tput sgr0 || true 2>/dev/null)"
    _arch="$(get_architecture || true)"
    cat <<EOF
Install ${PACKAGE_NAME} from https://github.com/${REPO_OWNER}/${REPO_NAME}

${_text_heading}Usage:${_text_reset}
  install.sh [OPTIONS]

${_text_heading}Options:${_text_reset}
  --arch          Override detected architecture [current: ${_arch}]
  --bin-dir       Override installation directory [default: ${BIN_DIR_DEFAULT}]
  --doc-dir       Override documentation directory [default: ${DOC_DIR_DEFAULT}]
  --license-dir   Override license directory [default: ${LIC_DIR_DEFAULT}]
    --runtime-dir   Override bundled runtime/plugins installation directory [default: ${RUNTIME_DIR_DEFAULT}]
  --sudo          Override command used to elevate privileges [default: ${SUDO_DEFAULT}]
  -h, --help      Print help
EOF
}

download_ee() {
    local _arch _dld _releases_url _releases _package_url _filename _package
    _arch="$1"

    if check_cmd curl; then
        _dld=curl
    elif check_cmd wget; then
        _dld=wget
    else
        need_cmd 'curl or wget'
    fi

    need_cmd grep
    _releases_url="https://api.github.com/repos/${REPO_OWNER}/${REPO_NAME}/releases/latest"

    case "${_dld}" in
        curl) _releases="$(curl -fsSL "${_releases_url}")" || err "curl: failed to download ${_releases_url}" ;;
        wget) _releases="$(wget -qO- "${_releases_url}")" || err "wget: failed to download ${_releases_url}" ;;
        *) err "unsupported downloader: ${_dld}" ;;
    esac

    echo "${_releases}" | grep -q 'API rate limit exceeded' &&
        err "GitHub API rate limit exceeded. Please try again later."

    _package_url="$(echo "${_releases}" | grep '"browser_download_url"' | cut -d '"' -f 4 | grep -- "${_arch}\.tar\.gz$")" ||
        err "${PACKAGE_NAME} has not been packaged for your architecture (${_arch})."

    _filename=$(basename "${_package_url}")
    _package="${_filename:-${PACKAGE_NAME}.tar.gz}"
    case "${_dld}" in
        curl) curl -fsSL -o "${_package}" "${_package_url}" || err "curl: failed to download ${_package_url}" ;;
        wget) wget -qO "${_package}" "${_package_url}" || err "wget: failed to download ${_package_url}" ;;
        *) err "unsupported downloader: ${_dld}" ;;
    esac

    CHECKSUM_URL="$(echo "${_releases}" | grep '"browser_download_url"' | cut -d '"' -f 4 | grep -- "$(basename "${_package_url}")\.sha256$" || true)"
    echo "${_package}"
}

maybe_verify_checksum() {
    local _package _checksum_file _expected _actual
    _package="$1"

    [ -n "${CHECKSUM_URL-}" ] || return 0

    _checksum_file="$(basename "${CHECKSUM_URL}")"
    if check_cmd curl; then
        curl -fsSL -o "${_checksum_file}" "${CHECKSUM_URL}" || err "curl: failed to download ${CHECKSUM_URL}"
    elif check_cmd wget; then
        wget -qO "${_checksum_file}" "${CHECKSUM_URL}" || err "wget: failed to download ${CHECKSUM_URL}"
    else
        return 0
    fi

    _expected="$(parse_checksum_file "${_checksum_file}" "$(basename "${_package}")")"
    [ -n "${_expected}" ] || return 0

    _actual="$(compute_sha256 "${_package}")"
    [ -n "${_actual}" ] || err "failed to compute SHA-256 for ${_package}"

    if [ "${_actual}" != "${_expected}" ]; then
        err "checksum mismatch for ${_package}: expected ${_expected}, got ${_actual}"
    fi

    echo "Verified SHA-256 for ${_package}"
}

parse_checksum_file() {
    local _file _target _line _sum _name
    _file="$1"
    _target="$2"
    while IFS= read -r _line; do
        [ -n "${_line}" ] || continue
        case "${_line}" in
            *" "*)
                _sum="$(printf '%s' "${_line}" | awk '{print $1}')"
                _name="$(printf '%s' "${_line}" | awk '{print $2}' | sed 's/^\*//')"
                [ "${_name}" = "${_target}" ] && { printf '%s' "${_sum}"; return 0; }
                ;;
            *)
                printf '%s' "${_line}"
                return 0
                ;;
        esac
    done <"${_file}"
    return 0
}

compute_sha256() {
    local _file
    _file="$1"
    if check_cmd sha256sum; then
        sha256sum "${_file}" | awk '{print $1}'
    elif check_cmd shasum; then
        shasum -a 256 "${_file}" | awk '{print $1}'
    elif check_cmd openssl; then
        openssl dgst -sha256 "${_file}" | awk '{print $NF}'
    else
        err "need one of: sha256sum, shasum, or openssl"
    fi
}

try_sudo() {
    if "$@" >/dev/null 2>&1; then
        return 0
    fi
    need_sudo
    "${SUDO}" "$@"
}

need_sudo() {
    if ! check_cmd "${SUDO}"; then
        err "could not find \`${SUDO}\`. Install sudo or rerun script with enough permissions."
    fi
    if ! "${SUDO}" -v; then
        err "sudo permissions not granted, aborting installation"
    fi
}

get_architecture() {
    local _ostype _cputype _bitness _arch _clibtype
    _ostype="$(uname -s)"
    _cputype="$(uname -m)"
    _clibtype="musl"

    if [ "${_ostype}" = Linux ]; then
        if [ "$(uname -o || true)" = Android ]; then
            _ostype=Android
        fi
    fi

    if [ "${_ostype}" = Darwin ] && [ "${_cputype}" = i386 ]; then
        if sysctl hw.optional.x86_64 | grep -q ': 1'; then
            _cputype=x86_64
        fi
    fi

    case "${_ostype}" in
        Android) _ostype=linux-android ;;
        Linux) check_proc; _ostype=unknown-linux-${_clibtype}; _bitness=$(get_bitness) ;;
        Darwin) _ostype=apple-darwin ;;
        *) err "unrecognized OS type: ${_ostype}" ;;
    esac

    case "${_cputype}" in
        i386|i486|i686|i786|x86) _cputype=i686 ;;
        arm64|aarch64) _cputype=aarch64 ;;
        x86_64|x86-64|x64|amd64) _cputype=x86_64 ;;
        *) err "unknown CPU type: ${_cputype}" ;;
    esac

    if [ "${_ostype}" = unknown-linux-musl ] && [ "${_bitness}" -eq 32 ]; then
        case "${_cputype}" in
            x86_64) _cputype=i686 ;;
            aarch64) _cputype=armv7 ;;
            *) ;;
        esac
    fi

    _arch="${_cputype}-${_ostype}"
    echo "${_arch}"
}

get_bitness() {
    need_cmd head
    local _current_exe_head
    _current_exe_head=$(head -c 5 /proc/self/exe)
    if [ "${_current_exe_head}" = "$(printf '\177ELF\001')" ]; then
        echo 32
    elif [ "${_current_exe_head}" = "$(printf '\177ELF\002')" ]; then
        echo 64
    else
        err "unknown platform bitness"
    fi
}

check_proc() {
    if ! test -L /proc/self/exe; then
        err "unable to find /proc/self/exe. Is /proc mounted?"
    fi
}

need_cmd() {
    if ! check_cmd "$1"; then
        err "need '$1' (command not found)"
    fi
}

check_cmd() {
    command -v -- "$1" >/dev/null 2>&1
}

ensure() {
    if ! "$@"; then
        err "command failed: $*"
    fi
}

assert_nz() {
    if [ -z "$1" ]; then
        err "found empty string: $2"
    fi
}

err() {
    echo "Error: $1" >&2
    exit 1
}

{ main "$@" || exit 1; }
