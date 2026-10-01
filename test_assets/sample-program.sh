#!/usr/bin/env bash
#
# Sample Bash script for editor syntax highlighting, code folding,
# bracket matching, and shell script navigation tests.
#

set -euo pipefail

readonly SCRIPT_NAME="$(basename "${BASH_SOURCE[0]}")"
readonly VERSION="1.4.2"
readonly DEFAULT_PORT=8080
readonly LOG_FILE="/tmp/${SCRIPT_NAME%.*}.log"

# Global state variables
VERBOSE=0
DRY_RUN=0
TARGET_HOST="localhost"
TARGET_PORT="${DEFAULT_PORT}"

# Color codes for terminal output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info() {
    printf "[%s] ${BLUE}INFO${NC}: %s\n" "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" "$*"
}

log_warn() {
    printf "[%s] ${YELLOW}WARN${NC}: %s\n" "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" "$*" >&2
}

log_err() {
    printf "[%s] ${RED}ERROR${NC}: %s\n" "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" "$*" >&2
}

cleanup() {
    local exit_code=$?
    log_info "Running cleanup hook (exit code: ${exit_code})..."
    rm -f "${LOG_FILE}.tmp" 2>/dev/null || true
    exit "${exit_code}"
}
trap cleanup EXIT INT TERM

show_help() {
    cat <<EOF
Usage: ${SCRIPT_NAME} [OPTIONS] <command> [arguments...]

A versatile deployment and inspection CLI utility.

Commands:
    status              Check service health and uptime
    deploy <artifact>   Deploy target package to configured host
    rollback <version>  Revert active deployment to previous state
    logs [-n lines]     Tail service logs

Options:
    -h, --help          Show this message and exit
    -v, --version       Display version information
    -H, --host HOST     Specify remote target host (default: localhost)
    -p, --port PORT     Specify remote target port (default: 8080)
    --verbose           Enable debug logging
    --dry-run           Print actions without executing them

Examples:
    ${SCRIPT_NAME} --host 192.168.1.50 --port 9000 status
    ${SCRIPT_NAME} --dry-run deploy /opt/builds/app-v2.1.tar.gz
EOF
}

check_dependencies() {
    local missing=0
    for cmd in curl tar grep awk; do
        if ! command -v "${cmd}" &>/dev/null; then
            log_err "Missing required system binary: ${cmd}"
            missing=$((missing + 1))
        fi
    done

    if [[ ${missing} -gt 0 ]]; then
        log_err "${missing} dependencies missing. Please install them to proceed."
        return 1
    fi
    return 0
}

deploy_artifact() {
    local artifact_path="${1:?Missing artifact path}"

    if [[ ! -f "${artifact_path}" ]]; then
        log_err "Artifact file does not exist: ${artifact_path}"
        return 2
    fi

    local checksum
    checksum="$(sha256sum "${artifact_path}" | awk '{print $1}')"
    log_info "Artifact SHA-256: ${checksum}"

    if [[ "${DRY_RUN}" -eq 1 ]]; then
        log_warn "[DRY-RUN] Would upload ${artifact_path} to ${TARGET_HOST}:${TARGET_PORT}"
        return 0
    fi

    log_info "Deploying ${artifact_path} to http://${TARGET_HOST}:${TARGET_PORT}/api/v1/deploy..."
    # Simulate deployment step
    sleep 0.2
    printf "${GREEN}SUCCESS:${NC} Deployment completed successfully.\n"
}

main() {
    local positional=()

    while [[ $# -gt 0 ]]; do
        case "$1" in
            -h|--help)
                show_help
                exit 0
                ;;
            -v|--version)
                echo "${SCRIPT_NAME} version ${VERSION}"
                exit 0
                ;;
            -H|--host)
                TARGET_HOST="$2"
                shift 2
                ;;
            -p|--port)
                TARGET_PORT="$2"
                shift 2
                ;;
            --verbose)
                VERBOSE=1
                shift
                ;;
            --dry-run)
                DRY_RUN=1
                shift
                ;;
            --)
                shift
                break
                ;;
            -*)
                log_err "Unknown option: $1"
                show_help >&2
                exit 1
                ;;
            *)
                positional+=("$1")
                shift
                ;;
        esac
    done

    if [[ ${#positional[@]} -eq 0 ]]; then
        log_err "No command specified."
        show_help >&2
        exit 1
    fi

    local cmd="${positional[0]}"
    check_dependencies

    case "${cmd}" in
        status)
            log_info "Checking health of ${TARGET_HOST}:${TARGET_PORT}..."
            echo '{"status": "ok", "uptime_seconds": 86400, "active_nodes": 4}'
            ;;
        deploy)
            local artifact="${positional[1]:-}"
            if [[ -z "${artifact}" ]]; then
                log_err "Missing argument: deploy requires an artifact file path"
                exit 1
            fi
            deploy_artifact "${artifact}"
            ;;
        logs)
            local lines="${positional[1]:-20}"
            log_info "Fetching recent ${lines} log lines from ${TARGET_HOST}..."
            ;;
        *)
            log_err "Unrecognized command: ${cmd}"
            exit 1
            ;;
    esac
}

main "$@"
