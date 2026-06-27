# Moraine vendored bash phase library: phase functions and eclass machinery.
#
# Faithfully ported from the stock Portage bin/ebuild.sh (inherit,
# EXPORT_FUNCTIONS, the E_* metadata fold) and bin/phase-functions.sh (the
# dispatcher, the pre/post hooks, and __ebuild_phase_funcs that binds the
# default_* set and the bare `default` command for an EAPI). The Rust phase
# driver sources this library, sources the ebuild so its top-level `inherit`
# runs, then calls __fold_eclass_metadata, __ebuild_phase_funcs, and
# __ebuild_phase_with_hooks for one phase.

# Convert the shell-quoted PORTAGE_ECLASS_LOCATIONS string the Rust driver
# exports into the array inherit() walks (matches ebuild.sh:610).
eval "PORTAGE_ECLASS_LOCATIONS=(${PORTAGE_ECLASS_LOCATIONS})"

# --- sandbox path helpers (ebuilds and eclasses may call these) -------------

__sb_append_var() {
    local _v=$1; shift
    local var="SANDBOX_${_v}"
    [[ $# -eq 1 ]] || die "Usage: add${_v,,} <path>"
    [[ ${1} == *:* ]] && die "add${_v,,} argument must not contain a colon"
    export ${var}="${!var:+${!var}:}$1"
}
addread()    { __sb_append_var READ    "$@"; }
addwrite()   { __sb_append_var WRITE   "$@"; }
adddeny()    { __sb_append_var DENY    "$@"; }
addpredict() { __sb_append_var PREDICT "$@"; }

# --- qa wrappers (simplified: no shopt/IFS QA check) ------------------------

__qa_source() {
    source "$@"
}

__qa_call() {
    "$@"
}

# --- inherit and EXPORT_FUNCTIONS (ported from ebuild.sh:220-407) ------------

declare -ix ECLASS_DEPTH=0
inherit() {
    ECLASS_DEPTH=$((${ECLASS_DEPTH} + 1))

    local -x ECLASS
    local __export_funcs_var
    local repo_location location potential_location x
    local B_IUSE B_REQUIRED_USE B_DEPEND B_RDEPEND B_PDEPEND
    local B_BDEPEND B_IDEPEND B_PROPERTIES B_RESTRICT
    while [[ "${1}" ]]; do
        location=""
        potential_location=""

        ECLASS="${1}"
        __export_funcs_var=__export_functions_${ECLASS_DEPTH}
        unset ${__export_funcs_var}

        for repo_location in "${PORTAGE_ECLASS_LOCATIONS[@]}"; do
            potential_location="${repo_location}/eclass/${1}.eclass"
            if [[ -f ${potential_location} ]]; then
                location="${potential_location}"
                debug-print "  eclass exists: ${location}"
                break
            fi
        done
        debug-print "inherit: ${1} -> ${location}"
        [[ -z ${location} ]] && die "${1}.eclass could not be found by inherit()"

        # Back up *DEPEND/IUSE so the eclass's own values can be captured.
        set -f
        unset B_IUSE B_REQUIRED_USE B_DEPEND B_RDEPEND B_PDEPEND
        unset B_BDEPEND B_IDEPEND B_PROPERTIES B_RESTRICT
        [[ -v IUSE         ]] && B_IUSE="${IUSE}"
        [[ -v REQUIRED_USE ]] && B_REQUIRED_USE="${REQUIRED_USE}"
        [[ -v DEPEND       ]] && B_DEPEND="${DEPEND}"
        [[ -v RDEPEND      ]] && B_RDEPEND="${RDEPEND}"
        [[ -v PDEPEND      ]] && B_PDEPEND="${PDEPEND}"
        [[ -v BDEPEND      ]] && B_BDEPEND="${BDEPEND}"
        [[ -v IDEPEND      ]] && B_IDEPEND="${IDEPEND}"
        unset IUSE REQUIRED_USE DEPEND RDEPEND PDEPEND BDEPEND IDEPEND
        if ___eapi_has_accumulated_PROPERTIES; then
            [[ -v PROPERTIES ]] && B_PROPERTIES=${PROPERTIES}
            unset PROPERTIES
        fi
        if ___eapi_has_accumulated_RESTRICT; then
            [[ -v RESTRICT ]] && B_RESTRICT=${RESTRICT}
            unset RESTRICT
        fi
        set +f

        __qa_source "${location}" || die "died sourcing ${location} in inherit()"

        set -f
        # Append each eclass-set value to the cumulative E_* variables.
        [[ -v IUSE         ]] && E_IUSE+="${E_IUSE:+ }${IUSE}"
        [[ -v REQUIRED_USE ]] && E_REQUIRED_USE+="${E_REQUIRED_USE:+ }${REQUIRED_USE}"
        [[ -v DEPEND       ]] && E_DEPEND+="${E_DEPEND:+ }${DEPEND}"
        [[ -v RDEPEND      ]] && E_RDEPEND+="${E_RDEPEND:+ }${RDEPEND}"
        [[ -v PDEPEND      ]] && E_PDEPEND+="${E_PDEPEND:+ }${PDEPEND}"
        [[ -v BDEPEND      ]] && E_BDEPEND+="${E_BDEPEND:+ }${BDEPEND}"
        [[ -v IDEPEND      ]] && E_IDEPEND+="${E_IDEPEND:+ }${IDEPEND}"

        [[ -v B_IUSE ]] && IUSE="${B_IUSE}"; [[ -v B_IUSE ]] || unset IUSE
        [[ -v B_REQUIRED_USE ]] && REQUIRED_USE="${B_REQUIRED_USE}"; [[ -v B_REQUIRED_USE ]] || unset REQUIRED_USE
        [[ -v B_DEPEND ]] && DEPEND="${B_DEPEND}"; [[ -v B_DEPEND ]] || unset DEPEND
        [[ -v B_RDEPEND ]] && RDEPEND="${B_RDEPEND}"; [[ -v B_RDEPEND ]] || unset RDEPEND
        [[ -v B_PDEPEND ]] && PDEPEND="${B_PDEPEND}"; [[ -v B_PDEPEND ]] || unset PDEPEND
        [[ -v B_BDEPEND ]] && BDEPEND="${B_BDEPEND}"; [[ -v B_BDEPEND ]] || unset BDEPEND
        [[ -v B_IDEPEND ]] && IDEPEND="${B_IDEPEND}"; [[ -v B_IDEPEND ]] || unset IDEPEND

        if ___eapi_has_accumulated_PROPERTIES; then
            [[ -v PROPERTIES ]] && E_PROPERTIES+=${E_PROPERTIES:+ }${PROPERTIES}
            [[ -v B_PROPERTIES ]] && PROPERTIES=${B_PROPERTIES}
            [[ -v B_PROPERTIES ]] || unset PROPERTIES
        fi
        if ___eapi_has_accumulated_RESTRICT; then
            [[ -v RESTRICT ]] && E_RESTRICT+=${E_RESTRICT:+ }${RESTRICT}
            [[ -v B_RESTRICT ]] && RESTRICT=${B_RESTRICT}
            [[ -v B_RESTRICT ]] || unset RESTRICT
        fi
        set +f

        if [[ -n ${!__export_funcs_var} ]]; then
            for x in ${!__export_funcs_var}; do
                debug-print "EXPORT_FUNCTIONS: ${x} -> ${ECLASS}_${x}"
                declare -F "${ECLASS}_${x}" >/dev/null || \
                    die "EXPORT_FUNCTIONS: ${ECLASS}_${x} is not defined"
                eval "$x() { ${ECLASS}_${x} \"\$@\" ; }" > /dev/null
            done
        fi
        unset $__export_funcs_var

        if ! contains_word "$1" "${INHERITED}"; then
            export INHERITED+=" $1"
        fi
        if [[ ${ECLASS_DEPTH} -eq 1 ]]; then
            export PORTAGE_EXPLICIT_INHERIT+=" $1"
        fi

        shift
    done
    ((--ECLASS_DEPTH))
    return 0
}

# Registers stub functions that call the eclass's functions, making them the
# defaults. Eval'd after the eclass is sourced, in inherit() above.
EXPORT_FUNCTIONS() {
    if [[ -z "${ECLASS}" ]]; then
        die "EXPORT_FUNCTIONS without a defined ECLASS"
    fi
    eval ${__export_funcs_var}+=\" $*\"
}

# Fold the accumulated eclass E_* metadata back into the ebuild's own metadata
# after the ebuild has been sourced (ported from ebuild.sh:661-683). This makes
# IUSE/*DEPEND/REQUIRED_USE/PROPERTIES/RESTRICT reflect eclass contributions.
__fold_eclass_metadata() {
    [[ -v EAPI ]] || EAPI=0
    export EAPI

    if ___eapi_has_RDEPEND_DEPEND_fallback; then
        export RDEPEND=${RDEPEND-${DEPEND}}
    fi

    IUSE+="${IUSE:+ }${E_IUSE}"
    DEPEND+="${DEPEND:+ }${E_DEPEND}"
    RDEPEND+="${RDEPEND:+ }${E_RDEPEND}"
    PDEPEND+="${PDEPEND:+ }${E_PDEPEND}"
    BDEPEND+="${BDEPEND:+ }${E_BDEPEND}"
    IDEPEND+="${IDEPEND:+ }${E_IDEPEND}"
    REQUIRED_USE+="${REQUIRED_USE:+ }${E_REQUIRED_USE}"

    if ___eapi_has_accumulated_PROPERTIES; then
        PROPERTIES+=${PROPERTIES:+ }${E_PROPERTIES}
    fi
    if ___eapi_has_accumulated_RESTRICT; then
        RESTRICT+=${RESTRICT:+ }${E_RESTRICT}
    fi

    unset E_IUSE E_REQUIRED_USE E_DEPEND E_RDEPEND E_PDEPEND
    unset E_BDEPEND E_IDEPEND E_PROPERTIES E_RESTRICT
}

# Map an EBUILD_PHASE short name to its phase function (used by econf's QA
# notice and __ebuild_phase_funcs callers).
__ebuild_arg_to_phase() {
    local phase_name=$1 phase_func
    case "${phase_name}" in
        pretend)   phase_func=pkg_pretend ;;
        setup)     phase_func=pkg_setup ;;
        nofetch)   phase_func=pkg_nofetch ;;
        unpack)    phase_func=src_unpack ;;
        prepare)   phase_func=src_prepare ;;
        configure) phase_func=src_configure ;;
        compile)   phase_func=src_compile ;;
        test)      phase_func=src_test ;;
        install)   phase_func=src_install ;;
        preinst)   phase_func=pkg_preinst ;;
        postinst)  phase_func=pkg_postinst ;;
        prerm)     phase_func=pkg_prerm ;;
        postrm)    phase_func=pkg_postrm ;;
        config)    phase_func=pkg_config ;;
        info)      phase_func=pkg_info ;;
        *)         phase_func=${phase_name} ;;
    esac
    echo "${phase_func}"
}

# --- dispatcher and hooks (ported from phase-functions.sh:247-261) ----------

__ebuild_phase() {
    # An undefined phase or hook is a no-op success; a defined one runs and its
    # exit status is propagated so the driver can detect a failed phase.
    declare -F "$1" >/dev/null || return 0
    __qa_call "$1"
}

__ebuild_phase_with_hooks() {
    local x phase_name=${1}
    for x in {pre_,,post_}${phase_name}; do
        __ebuild_phase "${x}"
    done
}

# Whether any phase up to and including the argument is in DEFINED_PHASES
# (ported from phase-functions.sh:413-421). Used by the S/WORKDIR fallback.
__has_phase_defined_up_to() {
    local phase
    for phase in unpack prepare configure compile test install; do
        contains_word "${phase}" "${DEFINED_PHASES}" && return 0
        [[ ${phase} == $1 ]] && return 1
    done
    return 1
}

# Change into the unpacked source directory for a src_* phase, applying the
# EAPI S/WORKDIR fallback and the empty-A rule (ported from the __dyn_* phase
# handlers, phase-functions.sh:431-439). The argument is the phase short name.
__cd_to_s() {
    local phase=$1
    if [[ -d ${S} ]]; then
        cd "${S}" || die "cd to S failed: ${S}"
    elif ___eapi_has_S_WORKDIR_fallback; then
        cd "${WORKDIR}" || die "cd to WORKDIR failed"
    elif [[ -z ${A} ]] && ! __has_phase_defined_up_to "${phase}"; then
        cd "${WORKDIR}" || die "cd to WORKDIR failed"
    else
        die "The source directory '${S}' doesn't exist"
    fi
}

# --- default_* and the bare default command --------------------------------
# Ported from phase-functions.sh:892-994. Given the EAPI and the phase function
# being run, this binds the default_* set, the bare `default`, and binds any
# phase the ebuild did not define to the default implementation.

__ebuild_phase_funcs() {
    [[ $# -ne 2 ]] && die "expected exactly 2 args, got $#: $*"

    local eapi=$1
    local phase_func=$2
    local all_phases="src_compile pkg_config src_configure pkg_info
        src_install pkg_nofetch pkg_postinst pkg_postrm pkg_preinst
        src_prepare pkg_prerm pkg_pretend pkg_setup src_test src_unpack"
    local x

    for x in ${all_phases}; do
        eval "default_${x}() {
            die \"default_${x}() is not supported in EAPI='${eapi}' in phase ${phase_func}\"
        }"
    done

    eval "default() {
        default_${phase_func}
    }"

    case "${eapi}" in
        0|1)
            for x in pkg_nofetch src_unpack src_test; do
                declare -F ${x} >/dev/null || \
                    eval "$x() { __eapi0_${x}; }"
            done
            if ! declare -F src_compile >/dev/null; then
                case "${eapi}" in
                    0) src_compile() { __eapi0_src_compile; } ;;
                    *) src_compile() { __eapi1_src_compile; } ;;
                esac
            fi
            ;;
        *)
            [[ ${phase_func} == pkg_nofetch ]] && \
                default_pkg_nofetch() { __eapi0_pkg_nofetch; }
            [[ ${phase_func} == src_unpack ]] && \
                default_src_unpack() { __eapi0_src_unpack; }
            [[ ${phase_func} == src_test ]] && \
                default_src_test() { __eapi0_src_test; }

            [[ ${phase_func} == src_prepare ]] && \
                default_src_prepare() { __eapi2_src_prepare; }
            [[ ${phase_func} == src_configure ]] && \
                default_src_configure() { __eapi2_src_configure; }
            [[ ${phase_func} == src_compile ]] && \
                default_src_compile() { __eapi2_src_compile; }

            declare -F pkg_nofetch >/dev/null   || pkg_nofetch() { default; }
            declare -F src_unpack >/dev/null    || src_unpack() { default; }
            declare -F src_prepare >/dev/null   || src_prepare() { default; }
            declare -F src_configure >/dev/null || src_configure() { default; }
            declare -F src_compile >/dev/null   || src_compile() { default; }
            declare -F src_test >/dev/null      || src_test() { default; }

            if [[ ${eapi} != [23] ]]; then
                [[ ${phase_func} == src_install ]] && \
                    default_src_install() { __eapi4_src_install; }
                declare -F src_install >/dev/null || src_install() { default; }
            fi

            if [[ ${eapi} != [2-5] ]]; then
                [[ ${phase_func} == src_prepare ]] && \
                    default_src_prepare() { __eapi6_src_prepare; }
                [[ ${phase_func} == src_install ]] && \
                    default_src_install() { __eapi6_src_install; }
                declare -F src_prepare >/dev/null || src_prepare() { default; }
            fi

            if [[ ${eapi} != [2-7] ]]; then
                [[ ${phase_func} == src_prepare ]] && \
                    default_src_prepare() { __eapi8_src_prepare; }
            fi
            ;;
    esac
}
