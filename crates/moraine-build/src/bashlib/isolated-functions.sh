# Moraine vendored bash phase library: isolated functions.
#
# Faithfully ported from the stock Portage bin/isolated-functions.sh. This is
# the low-level surface the helper and phase libraries build on: die/nonfatal/
# assert and the helper-die path, the has/contains_word predicates, and the
# elog message family.
#
# Two moraine patches are applied here and nowhere else:
#   1. The elog family (einfo/elog/ewarn/eerror/eqawarn) emits a tagged line
#      `MORAINE_ELOG <level> <phase> <message>` that the Rust phase driver scans
#      and classifies (see phase.rs::capture_elog), instead of the stock colored
#      stderr and ${T}/logging behavior.
#   2. has_version/best_version (in phase-helpers.sh) call ${MORAINE_IPC_HELPER}.

# --- predicates -------------------------------------------------------------

# Determines whether the first parameter is stringwise equal to any of the
# following parameters.
has() {
    local needle=$1
    shift

    local x
    for x in "$@"; do
        [[ "${x}" = "${needle}" ]] && return 0
    done
    return 1
}

hasq() {
    ___eapi_has_hasq || die "'${FUNCNAME}' banned in EAPI ${EAPI}"
    eqawarn "QA Notice: The 'hasq' function is deprecated (replaced by 'has')"
    has "$@"
}

hasv() {
    ___eapi_has_hasv || die "'${FUNCNAME}' banned in EAPI ${EAPI}"
    if has "$@"; then
        echo "$1"
        return 0
    fi
    return 1
}

# Considers the first parameter as a word and the second as a whitespace
# separated string, returning success when the word appears in the string.
contains_word() {
    local IFS
    [[ $1 == +([![:space:]]) && " ${*:2} " == *[[:space:]]"$1"[[:space:]]* ]]
}

# Strict IUSE membership test, backed by the IUSE_EFFECTIVE set the Rust driver
# exports. The stock implementation is generated per-package; here it is a thin
# wrapper that the use()/in_iuse strict checks consult.
___in_portage_iuse() {
    contains_word "$1" "${IUSE_EFFECTIVE}"
}

# --- pipestatus and quiet mode ---------------------------------------------

__pipestatus() {
    local status=( "${PIPESTATUS[@]}" ) s ret=0
    for s in "${status[@]}"; do
        [[ ${s} -ne 0 ]] && ret=${s}
    done
    return "${ret}"
}

__quiet_mode() {
    [[ ${PORTAGE_QUIET} -eq 1 ]]
}

__vecho() {
    __quiet_mode || echo "$@" >&2
}

# --- elog family (moraine MORAINE_ELOG tagging patch) -----------------------
# Messages are tagged so the Rust log-and-elog capture can classify them by
# severity. The driver scans the build log for these tags.

__elog_emit() {
    local level=$1; shift
    echo "MORAINE_ELOG ${level} ${EBUILD_PHASE:-unknown} $*"
}

einfo()    { __elog_emit INFO "$@"; return 0; }
einfon()   { __elog_emit INFO "$@"; return 0; }
elog()     { __elog_emit LOG "$@"; return 0; }
ewarn()    { __elog_emit WARN "$@"; return 0; }
eerror()   { __elog_emit ERROR "$@"; return 0; }
eqawarn()  { __elog_emit QA "$@"; return 0; }

# ebegin/eend are kept as minimal status helpers. The stock column formatting is
# not reproduced; eend forwards a failure message through eerror.
ebegin() {
    einfo "$* ..."
    return 0
}

eend() {
    local retval=${1:-0}
    shift
    if [[ ${retval} != 0 && -n $* ]]; then
        eerror "$*"
    fi
    return "${retval}"
}

# --- debug-print family -----------------------------------------------------

debug-print() {
    [[ ${EBUILD_PHASE} = depend || ! -d ${T} || ${#} -eq 0 ]] && return 0
    if [[ ${ECLASS_DEBUG_OUTPUT} == on ]]; then
        printf 'debug: %s\n' "${@}" >&2
    elif [[ -n ${ECLASS_DEBUG_OUTPUT} ]]; then
        printf 'debug: %s\n' "${@}" >> "${ECLASS_DEBUG_OUTPUT}"
    fi
    if [[ -w ${T} ]]; then
        printf '%s\n' "${@}" >> "${T}/eclass-debug.log"
    fi
}

debug-print-function() {
    debug-print "${1}: entering function, parameters: ${*:2}"
}

debug-print-section() {
    debug-print "now in section ${*}"
}

# --- death and nonfatal -----------------------------------------------------

nonfatal() {
    if ! ___eapi_has_nonfatal; then
        die "${FUNCNAME}() not supported in this EAPI"
    fi
    if [[ $# -lt 1 ]]; then
        die "${FUNCNAME}(): Missing argument"
    fi
    PORTAGE_NONFATAL=1 "$@"
}

# Helpers route their failures through __helpers_die. In EAPIs where helpers can
# die (4+) and PORTAGE_NONFATAL is not set, this escalates to die; otherwise it
# reports the message and returns the helper's exit status.
__helpers_die() {
    local retval=$?
    if ___eapi_helpers_can_die && [[ ${PORTAGE_NONFATAL} != 1 ]]; then
        die "$@"
    else
        echo -e "$@" >&2
        return "$(( retval ? retval : 1 ))"
    fi
}

# die honors -n/PORTAGE_NONFATAL in EAPIs that allow it, prints the failing
# package and phase, and aborts. The stock stack trace, death hooks, and IPC
# daemon signaling are intentionally not reproduced under moraine's boundary.
die() {
    local retval=$?

    if ___eapi_die_can_respect_nonfatal && [[ $1 == -n ]]; then
        shift
        if [[ ${PORTAGE_NONFATAL} == 1 ]]; then
            [[ $# -gt 0 ]] && echo -e "$@" >&2
            return "$(( retval ? retval : 1 ))"
        fi
    fi

    set +e
    local phase_str=
    [[ -n ${EBUILD_PHASE} ]] && phase_str=" (${EBUILD_PHASE} phase)"
    eerror "ERROR: ${CATEGORY}/${PF}::${PORTAGE_REPO_NAME} failed${phase_str}:"
    eerror "  ${*:-(no error message)}"
    echo "die: ${*:-(no error message)}" >&2
    exit 1
}
