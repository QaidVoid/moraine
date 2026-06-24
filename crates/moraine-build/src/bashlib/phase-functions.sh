# Moraine vendored bash phase library: phase function defaults.
#
# This is the boundary the Rust phase driver invokes. It is the moraine
# equivalent of the stock Portage bin/phase-functions.sh plus the EAPI-versioned
# default phase implementations from bin/phase-helpers.sh. The Rust side exports
# the full ebuild environment, sources the ebuild, sources this library, and then
# calls a single phase function by name.
#
# This vendored copy is intentionally minimal: it provides the default phase
# implementations and the helper-function surface contract that ebuilds and
# eclasses depend on. A production deployment ships the full lightly-patched fork
# of the stock bin/*.sh set here, selected by EAPI through the same ___eapi_*
# predicate mechanism the stock library uses. The Rust driver never reimplements
# any of this surface; it only schedules calls into it.

# --- EAPI predicates --------------------------------------------------------
# The driver exports EAPI; these predicates gate default behavior by EAPI level,
# matching the stock ___eapi_* helpers.

___eapi_has_src_prepare()   { [[ ${EAPI:-0} != 0 && ${EAPI:-0} != 1 ]]; }
___eapi_has_src_configure() { [[ ${EAPI:-0} != 0 && ${EAPI:-0} != 1 ]]; }
___eapi_has_pkg_pretend()   { [[ ${EAPI:-0} != 0 && ${EAPI:-0} != 1 && ${EAPI:-0} != 2 && ${EAPI:-0} != 3 ]]; }

# --- Default phase implementations -----------------------------------------
# Each default_* mirrors the EAPI default for the phase. An ebuild that defines
# the phase function overrides the default; otherwise the driver invokes the
# default_* function for that phase.

default_pkg_pretend() { :; }
default_pkg_setup()   { :; }

default_src_unpack() {
    if [[ -n ${A} ]]; then
        unpack ${A}
    fi
}

default_src_prepare() {
    if ___eapi_has_src_prepare; then
        # EAPI 6+ applies PATCHES and runs eapply_user; older EAPIs are a no-op
        # default. The full vendored library implements eapply/eapply_user here.
        if [[ -n ${PATCHES} ]]; then
            eapply "${PATCHES[@]}"
        fi
        eapply_user
    fi
}

default_src_configure() {
    if ___eapi_has_src_configure; then
        if [[ -x ${ECONF_SOURCE:-.}/configure ]]; then
            econf
        fi
    fi
}

default_src_compile() {
    if [[ -f Makefile || -f GNUmakefile || -f makefile ]]; then
        emake || die "emake failed"
    fi
}

default_src_test() {
    # The default test phase runs the make check/test target when present.
    if make -n check >/dev/null 2>&1; then
        emake check
    elif make -n test >/dev/null 2>&1; then
        emake test
    fi
}

default_src_install() {
    if [[ -f Makefile || -f GNUmakefile || -f makefile ]]; then
        emake DESTDIR="${D}" install
    fi
}

default_pkg_nofetch() {
    [[ -n ${A} ]] && einfo "The following files must be fetched manually: ${A}"
}

# --- Phase dispatcher -------------------------------------------------------
# The driver calls __ebuild_phase <func>. It runs the ebuild-defined function if
# present, otherwise the default_<func>. This is the single entry point the Rust
# driver invokes per forked process.

__ebuild_phase() {
    local func=$1
    if declare -F "${func}" >/dev/null 2>&1; then
        "${func}"
    elif declare -F "default_${func}" >/dev/null 2>&1; then
        "default_${func}"
    else
        return 0
    fi
}
